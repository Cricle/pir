#!/usr/bin/env node
/**
 * P3 real-task E2E (WAVE-REPORT.md §2 evidence): drive a small REAL engineering task through
 * a REAL model gateway and record wall-clock timeline, turns, tool calls and
 * tokens — for the `pir` binary and, optionally, the reference `pi` CLI on the
 * same fixture. Verdict = the fixture's own `node --test` passes afterwards.
 *
 * Hermetic by construction:
 *  - a PER-LEG isolated agent dir (a fresh temp copy of `models.json` etc.) via
 *    PIR_AGENT_DIR / PI_CODING_AGENT_DIR, so no session state or cache from the
 *    user's `~/.pi` is read or written, no user extensions load, and one leg can
 *    never see state the other leg wrote;
 *  - every temp agent dir (each holds a credentials copy) is deleted in a
 *    `finally`, even on failure/interrupt;
 *  - the fixtures are dependency-free CommonJS projects under the output dir.
 *
 * Usage:
 *   node scripts/perf/p3_task_e2e.mjs [--cli <pir.exe>] [--pi-cli <pi cmd>|--no-pi]
 *       [--model <provider/id>] [--task bug|feature|both] [--runs N]
 *       [--out <dir>] [--timeout-ms N] [--dry-run] [--keep-agent-dir]
 *   node scripts/perf/p3_task_e2e.mjs --help
 *
 * Standalone runs are hermetic per leg; `ab-ar/run_ab.mjs` remains the fully
 * isolated path (throwaway HOME + persistent per-leg launcher dirs), and it is
 * the fair A/B path because pi also reads `~/.agents/skills`.
 *
 * Exit code: 0 when every task verdict passed (dry-run: fixture pre-checks ok),
 * 3 when a task verdict failed, 2 on a harness error.
 */

import { execFile, spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const args = {
    cli: null,
    // Same default as ab_bird: Node's spawn() without a shell does not apply
    // PATHEXT, so a bare "pi" resolves to the Unix shim (or nothing) and every
    // pi leg dies in ~3 ms with zero turns (measured). The npm .cmd shim is
    // the Windows entry point.
    piCli: process.platform === "win32"
      ? join(process.env.APPDATA ?? "", "npm", "pi.cmd")
      : "pi",
    noPi: false,
    model: "openai/gpt-5",
    task: "both",
    runs: 1,
    out: null,
    timeoutMs: 900_000,
    dryRun: false,
    keepAgentDir: false,
    force: false,
    help: false,
    allowStale: false,
    stallTtftMs: 10_000,
    agentDir: null,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    const next = () => {
      i += 1;
      if (i >= argv.length) throw new Error(`${flag} needs a value`);
      return argv[i];
    };
    switch (flag) {
      case "--cli":
        args.cli = next();
        break;
      case "--pi-cli":
        args.piCli = next();
        break;
      case "--no-pi":
        args.noPi = true;
        break;
      case "--model":
        args.model = next();
        break;
      case "--task":
        args.task = next();
        break;
      case "--runs":
        args.runs = Number.parseInt(next(), 10);
        break;
      case "--out":
        args.out = next();
        break;
      case "--timeout-ms":
        args.timeoutMs = Number.parseInt(next(), 10);
        break;
      case "--dry-run":
        args.dryRun = true;
        break;
      case "--keep-agent-dir":
        args.keepAgentDir = true;
        break;
      case "--force":
        args.force = true;
        break;
      case "--allow-stale":
        args.allowStale = true;
        break;
      case "--stall-ttft-ms":
        args.stallTtftMs = Number.parseInt(next(), 10);
        break;
      case "--agent-dir":
        args.agentDir = next();
        break;
      case "-h":
      case "--help":
        args.help = true;
        break;
      default:
        throw new Error(`unknown flag: ${flag} (see --help)`);
    }
  }
  if (!["bug", "feature", "both"].includes(args.task)) throw new Error(`bad --task ${args.task}`);
  if (!Number.isInteger(args.runs) || args.runs < 1) throw new Error(`--runs must be a positive integer, got ${args.runs}`);
  if (!Number.isInteger(args.stallTtftMs) || args.stallTtftMs < 1) throw new Error(`--stall-ttft-ms must be a positive integer, got ${args.stallTtftMs}`);
  // `--cli` is only defaulted here (never resolved/existence-checked) so that
  // `--dry-run` and `--help` work on a machine without either binary.
  if (!args.cli) {
    const release = join(REPO, "target-alt", "release", "pir.exe");
    const debug = join(REPO, "target-alt", "debug", "pir.exe");
    args.cli = existsSync(release) ? release : debug;
  }
  if (!args.out) {
    const stamp = new Date().toISOString().replace(/[-:]/g, "").replace(/\..+$/, "").replace("T", "-");
    // Per-process too: concurrent standalone runs must not share an out dir.
    args.out = join(tmpdir(), "pir-p3-e2e", `${stamp}-${process.pid}`);
  }
  return args;
}

const USAGE = [
  "Usage: node scripts/perf/p3_task_e2e.mjs [flags]",
  "",
  "  --cli <exe>          pir binary (default target-alt/release|debug/pir.exe)",
  "  --pi-cli <cmd>       upstream pi entry point (default %APPDATA%/npm/pi.cmd)",
  "  --no-pi              run only the pir leg",
  "  --model <provider/id> model id (default openai/gpt-5)",
  "  --task bug|feature|both  fixture set (default both)",
  "  --runs N             independent repetitions per task (default 1)",
  "  --out <dir>          artifacts dir (outside the repo; empty unless --force)",
  "  --timeout-ms N       per-leg timeout (default 900000)",
  "  --force              replace a non-empty --out dir",
  "  --allow-stale        accept a release pir older than src/ (frozen build)",
  "  --stall-ttft-ms N    TTFT over this marks a run non-stall-free (default 10000)",
  "  --agent-dir <dir>    source agent config dir (default ~/.pi/agent);",
  "                       each leg still gets its own fresh copy",
  "  --dry-run            print the plan and run fixture pre-checks; no model call",
  "  --keep-agent-dir     keep the per-leg agent dirs (they hold credentials)",
  "  -h, --help           this text",
  "",
  "Verdict = the fixture's pristine `node --test` passes after the leg.",
  "Each leg gets its own agent dir, work dir and fixture copy; nothing is shared.",
  "Exit: 0 all PASS · 3 verdict FAIL · 2 harness error.",
].join("\n");

// ---------------------------------------------------------------------------
// Fixtures (dependency-free CommonJS + node:test)
// ---------------------------------------------------------------------------

const FIXTURES = {
  bug: {
    // Off-by-one: the loop drops the last element of the range.
    files: {
      "package.json": `{ "name": "p3-bug", "private": true }\n`,
      "calc.js": [
        "// Sum of every integer in [from, to] (inclusive).",
        "function sumRange(from, to) {",
        "  let sum = 0;",
        "  for (let n = from; n < to; n += 1) {",
        "    sum += n;",
        "  }",
        "  return sum;",
        "}",
        "",
        "module.exports = { sumRange };",
        "",
      ].join("\n"),
      "calc.test.js": [
        "const test = require('node:test');",
        "const assert = require('node:assert');",
        "const { sumRange } = require('./calc.js');",
        "",
        "test('sumRange is inclusive on both ends', () => {",
        "  assert.strictEqual(sumRange(1, 5), 15);",
        "  assert.strictEqual(sumRange(3, 3), 3);",
        "  assert.strictEqual(sumRange(0, 0), 0);",
        "});",
        "",
      ].join("\n"),
    },
    prompt:
      "The test in calc.test.js fails. Find and fix the bug in calc.js so the test passes, " +
      "then run `node --test` in this directory to confirm. Do not change the test file.",
  },
  feature: {
    // Implement a duration parser to a given spec (tests exist and must pass).
    files: {
      "package.json": `{ "name": "p3-feature", "private": true }\n`,
      "duration.js": [
        "// Parse '<n>h<n>m<n>s' (units optional, any subset, order h/m/s) into seconds.",
        "// Examples: '1h30m' -> 5400, '90s' -> 90, '2h' -> 7200, '1h2m3s' -> 3723.",
        "function parseDuration(text) {",
        "  throw new Error('not implemented');",
        "}",
        "",
        "module.exports = { parseDuration };",
        "",
      ].join("\n"),
      "duration.test.js": [
        "const test = require('node:test');",
        "const assert = require('node:assert');",
        "const { parseDuration } = require('./duration.js');",
        "",
        "test('parseDuration handles the documented shapes', () => {",
        "  assert.strictEqual(parseDuration('1h30m'), 5400);",
        "  assert.strictEqual(parseDuration('90s'), 90);",
        "  assert.strictEqual(parseDuration('2h'), 7200);",
        "  assert.strictEqual(parseDuration('1h2m3s'), 3723);",
        "});",
        "",
        "test('parseDuration rejects garbage', () => {",
        "  assert.throws(() => parseDuration('nope'));",
        "  assert.throws(() => parseDuration(''));",
        "});",
        "",
      ].join("\n"),
    },
    prompt:
      "Implement parseDuration in duration.js to the specification in its comment so the tests in " +
      "duration.test.js pass, then run `node --test` in this directory to confirm. " +
      "Do not change the test file.",
  },
};

function materializeFixture(root, name) {
  const dir = join(root, "work", name);
  mkdirSync(dir, { recursive: true });
  for (const [file, content] of Object.entries(FIXTURES[name].files)) {
    writeFileSync(join(dir, file), content);
  }
  return dir;
}

function snapshot(dir, task) {
  const out = {};
  for (const file of Object.keys(FIXTURES[task].files)) {
    const path = join(dir, file);
    // A real agent may delete a fixture file; record that instead of crashing
    // the entire campaign before the report is written.
    out[file] = existsSync(path) ? readFileSync(path, "utf8") : null;
  }
  return out;
}

function testFiles(task) {
  return Object.keys(FIXTURES[task].files).filter((file) => /\.test\.js$/i.test(file));
}

function testsArePristine(dir, task) {
  return testFiles(task).every((file) => {
    const path = join(dir, file);
    return existsSync(path) && readFileSync(path, "utf8") === FIXTURES[task].files[file];
  });
}

function prepareOutDir(outDir, force) {
  const resolved = resolve(outDir);
  const repo = resolve(REPO);
  const key = (value) => (process.platform === "win32" ? value.toLowerCase() : value);
  const r = key(resolved);
  const repoKey = key(repo);
  if (resolved === resolve(dirname(resolved))
      || r === repoKey
      || r.startsWith(`${repoKey}${sep}`)
      || repoKey.startsWith(`${r}${sep}`)) {
    throw new Error(`refusing unsafe --out path (repo or inside it, or a drive root): ${resolved}`);
  }
  if (existsSync(resolved)) {
    const entries = readdirSync(resolved);
    if (entries.length > 0 && !force) {
      throw new Error(`--out is not empty: ${resolved} (pass --force to overwrite)`);
    }
    if (force) {
      for (const entry of entries) {
        if (entry === "work" || entry === "report.md" || entry === "report.json"
            || /^events-/.test(entry) || /^stderr-/.test(entry) || /^artifacts-/.test(entry)) {
          rmSync(join(resolved, entry), { recursive: true, force: true });
        }
      }
    }
  } else {
    mkdirSync(resolved, { recursive: true });
  }
}

function hashFileSha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function runBinaryVersion(path) {
  const isCmdShim = process.platform === "win32" && /\.(cmd|bat)$/i.test(path);
  const program = isCmdShim ? process.env.ComSpec || "cmd.exe" : path;
  const argv = isCmdShim ? ["/d", "/c", path, "--version"] : ["--version"];
  const result = spawnSync(program, argv, { encoding: "utf8", timeout: 15_000, windowsHide: true });
  const stdout = `${result.stdout ?? ""}`.trim();
  return {
    status: result.status ?? null,
    version: result.status === 0
      ? stdout.split(/\r?\n/).map((line) => line.trim()).filter(Boolean)[0] ?? null
      : null,
  };
}

function resolveCmdLauncher(path) {
  if (process.platform !== "win32" || !/\.(cmd|bat)$/i.test(path)) return null;
  let text;
  try { text = readFileSync(path, "utf8"); } catch { return null; }
  for (const line of text.split(/\r?\n/)) {
    const match = /^\s*(?:call\s+)?"([^"%]+)"\s+%\*\s*$/i.exec(line.trim());
    if (!match) continue;
    const target = resolve(dirname(path), match[1]);
    if (existsSync(target)) return target;
  }
  return null;
}

function binaryInfo(label, path) {
  const requestedPath = resolve(path);
  const info = { label, requestedPath, path: requestedPath, exists: existsSync(requestedPath) };
  if (!info.exists) return info;
  const target = resolveCmdLauncher(requestedPath);
  if (target && target !== requestedPath) {
    info.launcher = requestedPath;
    info.path = target;
  }
  try { info.realpath = realpathSync(info.path); } catch { info.realpath = info.path; }
  try { info.mtime = statSync(info.realpath).mtime.toISOString(); } catch {}
  try { info.sha256 = hashFileSha256(info.realpath); } catch { info.sha256 = null; }
  const version = runBinaryVersion(info.realpath);
  info.version = version.version;
  return info;
}

function assertDistinctBinaries(pirInfo, piInfo) {
  if (!pirInfo.exists || !piInfo.exists) return;
  if (pirInfo.sha256 && piInfo.sha256 && pirInfo.sha256 === piInfo.sha256) {
    throw new Error(`pir and pi resolve to identical binaries (sha256 ${pirInfo.sha256})`);
  }
  const same = (a, b) => (process.platform === "win32" ? a.toLowerCase() === b.toLowerCase() : a === b);
  if (pirInfo.realpath && piInfo.realpath && same(pirInfo.realpath, piInfo.realpath)) {
    throw new Error(`pir and pi resolve to the same path: ${pirInfo.realpath}`);
  }
}

function newestMtimeMs(target) {
  let newest = 0;
  const visit = (path) => {
    let stat;
    try { stat = statSync(path); } catch { return; }
    if (stat.isDirectory()) {
      for (const entry of readdirSync(path, { withFileTypes: true })) {
        if (entry.name === ".git" || entry.name === "node_modules" || entry.name === "target" || entry.name === "target-alt") continue;
        visit(join(path, entry.name));
      }
    } else if (stat.isFile()) {
      newest = Math.max(newest, stat.mtimeMs);
    }
  };
  visit(target);
  return newest;
}

function checkReleaseFreshness(pirInfo, allowStale) {
  if (!pirInfo.exists || !pirInfo.realpath || !/(^|[\\/])release([\\/]|$)/i.test(pirInfo.realpath)) {
    return { checked: false, reason: "not a release path" };
  }
  const newestSource = Math.max(
    newestMtimeMs(join(REPO, "src")),
    newestMtimeMs(join(REPO, "Cargo.toml")),
    newestMtimeMs(join(REPO, "Cargo.lock")),
  );
  const binaryMtime = pirInfo.mtime ? Date.parse(pirInfo.mtime) : 0;
  const stale = newestSource > binaryMtime + 1_000;
  const record = {
    checked: true,
    stale,
    allowStale: Boolean(allowStale),
    binaryMtime: pirInfo.mtime ?? null,
    newestSourceMtime: newestSource > 0 ? new Date(newestSource).toISOString() : null,
  };
  if (stale && !allowStale) {
    throw new Error(
      `pir release binary is older than src/Cargo.toml/Cargo.lock: ${pirInfo.realpath} ` +
      `(${record.binaryMtime} < ${record.newestSourceMtime}); rebuild or pass --allow-stale`,
    );
  }
  return record;
}

/** Peak working set of the whole process tree (MB), sampled every 3 s. */
function samplePeakTreeRssMb(child, state) {
  if (process.platform !== "win32") return;
  const script =
    "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ProcessId) $($_.ParentProcessId) $($_.WorkingSetSize)\" }";
  state.timer = setInterval(() => {
    execFile("powershell", ["-NoProfile", "-Command", script], { windowsHide: true, timeout: 5000 },
      (error, stdout) => {
        if (error || !stdout) return;
        const rows = String(stdout).trim().split(/\r?\n/)
          .map((line) => line.trim().split(/\s+/).map(Number))
          .filter((row) => row.length === 3 && row.every((n) => Number.isFinite(n)));
        if (rows.length === 0) return;
        const children = new Map();
        const own = new Map();
        for (const [pid, ppid, ws] of rows) {
          own.set(pid, ws);
          if (!children.has(ppid)) children.set(ppid, []);
          children.get(ppid).push(pid);
        }
        let total = own.get(child.pid) ?? 0;
        const stack = [child.pid];
        while (stack.length) {
          const cur = stack.pop();
          for (const kid of children.get(cur) ?? []) {
            total += own.get(kid) ?? 0;
            stack.push(kid);
          }
        }
        const mb = total / (1024 * 1024);
        if (Number.isFinite(mb)) state.peakMb = Math.max(state.peakMb, mb);
      });
  }, 3000);
}

function median(values) {
  const nums = values.filter((v) => Number.isFinite(v)).sort((a, b) => a - b);
  if (nums.length === 0) return null;
  const mid = Math.floor(nums.length / 2);
  return nums.length % 2 === 1 ? nums[mid] : Math.round((nums[mid - 1] + nums[mid]) / 2);
}

function archiveDir(source, destination) {
  rmSync(destination, { recursive: true, force: true });
  mkdirSync(dirname(destination), { recursive: true });
  cpSync(source, destination, { recursive: true });
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

function runSync(command, args, options) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    windowsHide: true,
    shell: process.platform === "win32" && /\.(cmd|bat)$/i.test(command),
    ...options,
  });
  return {
    status: result.status,
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? "",
    error: result.error ? String(result.error.message ?? result.error) : null,
  };
}

/**
 * Run the fixture's own test suite; verdict = exit code 0. Bounded like
 * ab_bird's runner: `spawnSync({timeout})` cannot kill a process tree on
 * Windows, and a hung test would otherwise wedge the whole campaign.
 */
function runFixtureTests(dir, timeoutMs = 180_000, perTestMs = 30_000) {
  return new Promise((resolveDone) => {
    const child = spawn("node", ["--test", "--test-reporter=tap", `--test-timeout=${perTestMs}`], {
      cwd: dir,
      windowsHide: true,
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, NODE_OPTIONS: "" },
    });
    let stdout = "";
    let stderr = "";
    let settled = false;
    let timedOut = false;
    let killTimer = null;
    const finish = (status) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (killTimer !== null) clearTimeout(killTimer);
      resolveDone({ status, stdout, stderr, timedOut });
    };
    const timer = setTimeout(() => {
      timedOut = true;
      if (process.platform === "win32") {
        spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true, timeout: 30_000 });
      } else {
        try { child.kill("SIGKILL"); } catch {}
      }
      killTimer = setTimeout(() => {
        try { child.stdout?.destroy(); } catch {}
        try { child.stderr?.destroy(); } catch {}
        finish(124);
      }, 5_000);
    }, timeoutMs);
    child.stdout.on("data", (chunk) => { stdout += chunk.toString("utf8"); });
    child.stderr.on("data", (chunk) => { stderr += chunk.toString("utf8"); });
    child.on("error", (error) => {
      stderr += String(error?.message ?? error);
      finish(null);
    });
    // A survivor holding inherited stdio pipes must not keep `close` from
    // firing forever after the root test process exits.
    child.on("exit", () => {
      setTimeout(() => {
        try { child.stdout?.destroy(); } catch {}
        try { child.stderr?.destroy(); } catch {}
      }, 5_000);
    });
    child.on("close", (code) => finish(code));
  });
}

/**
 * Run the pristine copy of the tests, never the agent-edited ones. The work
 * dir is copied and the fixture's test files are overwritten before `node
 * --test`, so a model that weakens the visible suite cannot earn a PASS.
 */
async function runPristineFixtureTests(dir, task) {
  const copy = mkdtempSync(join(tmpdir(), `pir-p3-verdict-${task}-`));
  try {
    cpSync(dir, copy, { recursive: true });
    for (const file of testFiles(task)) {
      writeFileSync(join(copy, file), FIXTURES[task].files[file]);
    }
    return await runFixtureTests(copy);
  } finally {
    rmSync(copy, { recursive: true, force: true });
  }
}

// ---------------------------------------------------------------------------
// One task run (one CLI, one fixture)
// ---------------------------------------------------------------------------

function runTask({ cli, label, fixtureDir, prompt, model, timeoutMs, outDir, dryRun, childEnv }) {
  const eventsPath = join(outDir, `events-${label}.jsonl`);
  const stderrPath = join(outDir, `stderr-${label}.log`);
  const meta = {
    label,
    cli,
    task: fixtureDir.split(/[\\/]/).pop(),
    model,
    dryRun,
  };
  if (dryRun) {
    meta.command = [cli, "--mode", "json", "--print", "--model", model, "--no-session", `<prompt:${prompt.length} chars>`];
    return { meta, events: [], metrics: null };
  }

  const started = Date.now();
  // `.cmd`/`.bat` shims go through cmd.exe in ARGV form: Node's `shell: true`
  // builds a single quoted line (`cmd /d /s /c "<prog> args"`) which fails for
  // an npm shim path ("filename/directory syntax is incorrect", measured on
  // `pi.cmd`), while separate argv entries work (rc=0).
  const isCmdShim = process.platform === "win32" && /\.(cmd|bat)$/i.test(cli);
  const program = isCmdShim ? process.env.ComSpec || process.env.COMSPEC || "cmd.exe" : cli;
  // Keep the prompt off argv: cmd.exe mangles long/quote-heavy text through
  // the npm .cmd shim. Both CLIs join piped stdin with the initial message.
  const cliArgs = ["--mode", "json", "--print", "--model", model, "--no-session"];
  const spawnArgs = isCmdShim ? ["/d", "/c", cli, ...cliArgs] : cliArgs;
  const child = spawn(program, spawnArgs, {
    cwd: fixtureDir,
    windowsHide: true,
    env: childEnv,
    stdio: ["pipe", "pipe", "pipe"],
  });
  const rss = { peakMb: 0, timer: null };
  if (child.pid !== undefined) samplePeakTreeRssMb(child, rss);
  if (child.stdin) {
    child.stdin.on("error", () => { /* verdict reports a failed leg */ });
    try { child.stdin.end(prompt); } catch {}
  }

  const events = [];
  const stderrChunks = [];
  let firstLineAt = null;
  let requestAt = null;
  let ttftAt = null;
  let buffer = "";
  let killedByTimeout = false;
  let settleDone = null;
  let forceTimer = null;

  const timer = setTimeout(() => {
    killedByTimeout = true;
    if (process.platform === "win32") {
      spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true });
    } else {
      try { child.kill("SIGKILL"); } catch {}
    }
    // Even if taskkill fails/misses a descendant holding the stdio pipes,
    // resolve the run as a timed-out 124 after a bounded grace period.
    forceTimer = setTimeout(() => {
      try { child.stdout?.destroy(); } catch {}
      try { child.stderr?.destroy(); } catch {}
      if (rss.timer) clearInterval(rss.timer);
      if (settleDone) settleDone({ code: 124, forcedTimeout: true });
    }, 5_000);
  }, timeoutMs);

  child.stdout.on("data", (chunk) => {
    buffer += chunk.toString("utf8");
    let index;
    while ((index = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, index).trim();
      buffer = buffer.slice(index + 1);
      if (!line) continue;
      if (firstLineAt === null) firstLineAt = Date.now();
      let parsed = null;
      try {
        parsed = JSON.parse(line);
      } catch {
        parsed = { type: "_unparsed", line: line.slice(0, 500) };
      }
      const kind = parsed.assistantEvent ?? parsed.assistantMessageEvent?.type ?? null;
      const delta = parsed.assistantMessageEvent?.delta ?? parsed.delta;
      if (requestAt === null && parsed.type === "message_update" && kind === "start") {
        requestAt = Date.now();
      }
      if (ttftAt === null && parsed.type === "message_update"
          && kind && /_delta$/.test(kind) && typeof delta === "string") {
        ttftAt = Date.now();
      }
      parsed._at = Date.now() - started;
      events.push(parsed);
    }
  });
  child.stderr.on("data", (chunk) => stderrChunks.push(chunk.toString("utf8")));

  const done = new Promise((resolveDone) => {
    settleDone = resolveDone;
    child.on("error", (error) => {
      if (forceTimer !== null) clearTimeout(forceTimer);
      if (rss.timer) clearInterval(rss.timer);
      resolveDone({ spawnError: String(error.message ?? error) });
    });
    child.on("close", (code) => {
      if (forceTimer !== null) clearTimeout(forceTimer);
      if (rss.timer) clearInterval(rss.timer);
      resolveDone({ code });
    });
  });

  return {
    child, done, events, stderrChunks, started, startedRef: started, rss,
    get firstLineAt() { return firstLineAt; },
    get requestAt() { return requestAt; },
    get ttftAt() { return ttftAt; },
    get killedByTimeout() { return killedByTimeout; },
    clearTimer: () => {
      clearTimeout(timer);
      if (forceTimer !== null) clearTimeout(forceTimer);
    },
    eventsPath, stderrPath, fixtureDir, prompt, meta, outDir, model,
  };
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

function summarize(events, started, closeInfo, firstLineAt, requestAt, ttftAt, killedByTimeout) {
  const metrics = {
    wallMs: null,
    startupMs: firstLineAt === null ? null : firstLineAt - started,
    requestAtMs: requestAt === null ? null : requestAt - started,
    ttftMs: ttftAt === null ? null : ttftAt - started,
    ttftFromRequestMs: ttftAt === null || requestAt === null || ttftAt < requestAt
      ? null
      : ttftAt - requestAt,
    exitCode: closeInfo.code ?? null,
    spawnError: closeInfo.spawnError ?? null,
    timedOut: killedByTimeout,
    turns: 0,
    toolCalls: [],
    toolErrors: 0,
    usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0, totalTokens: 0 },
    assistantMessages: 0,
    backgroundEnds: 0,
    unparsedLines: 0,
    peakRssMb: 0,
    turnSpans: [],
  };
  const openTurns = new Map();
  for (const event of events) {
    switch (event.type) {
      case "turn_start":
        openTurns.set(event.turn, event._at);
        break;
      case "turn_end": {
        const from = openTurns.get(event.turn);
        if (from !== undefined) metrics.turnSpans.push({ turn: event.turn, ms: event._at - from });
        break;
      }
      case "tool_execution_end":
        metrics.toolCalls.push({
          turn: event.turn,
          toolName: event.toolName,
          durationMs: event.durationMs ?? null,
          isError: Boolean(event.isError),
          atMs: event._at,
        });
        if (event.isError) metrics.toolErrors += 1;
        break;
      case "message_end": {
        const message = event.message ?? {};
        if (message.role === "assistant") {
          metrics.assistantMessages += 1;
          const usage = message.usage ?? {};
          metrics.usage.input += usage.input ?? 0;
          metrics.usage.output += usage.output ?? 0;
          metrics.usage.cacheRead += usage.cacheRead ?? 0;
          metrics.usage.cacheWrite += usage.cacheWrite ?? 0;
          metrics.usage.reasoning += usage.reasoning ?? 0;
          metrics.usage.totalTokens += usage.totalTokens ?? 0;
        }
        break;
      }
      case "background_tool_end":
        metrics.backgroundEnds += 1;
        break;
      case "_unparsed":
        metrics.unparsedLines += 1;
        break;
      default:
        break;
    }
  }
  // Count turn_start events, not distinct turn ids: upstream pi omits the
  // `turn` field, and counting ids collapsed multi-turn runs to 1.
  metrics.turns = events.filter((e) => e.type === "turn_start").length;
  return metrics;
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

function fmt(value, suffix = "") {
  return value === null || value === undefined ? "—" : `${value}${suffix}`;
}

function writeReport(outDir, runs, config) {
  const lines = [];
  lines.push("# P3 real-task E2E (WAVE-REPORT.md §2)");
  lines.push("");
  lines.push(`- 时间：${new Date().toISOString()}`);
  lines.push(`- 模型：\`${config.model}\`（同一模型网关，凭证经临时 agent dir 传入，未打印）`);
  lines.push(`- agent dir 隔离：yes（临时目录，仅拷贝 models.json/settings.json/auth.json；运行后删除）`);
  lines.push(`- dry run：${config.dryRun ? "yes（未调用模型）" : "no"}`);
  lines.push("");
  lines.push("| CLI | 任务 | 判定 | 墙钟 | 启动 | TTFT | TTFT(req) | turns | tool calls | 工具错误 | tokens(in/out/total) | 退出码 |");
  lines.push("|---|---|---|---|---|---|---|---|---|---|---|---|");
  for (const run of runs) {
    const m = run.metrics;
    const verdict = run.verdict;
    lines.push(
      `| ${run.label} | ${run.task} | ${verdict} | ${fmt(m?.wallMs, "ms")} | ${fmt(m?.startupMs, "ms")} | ${fmt(m?.ttftMs, "ms")} | ${fmt(m?.ttftFromRequestMs, "ms")} | ${fmt(m?.turns)} | ${fmt(m?.toolCalls.length)} | ${fmt(m?.toolErrors)} | ${m ? `${m.usage.input}/${m.usage.output}/${m.usage.totalTokens}` : "—"} | ${fmt(m?.exitCode)} |`,
    );
  }
  lines.push("");
  for (const run of runs) {
    lines.push(`## ${run.label} · ${run.task}`);
    lines.push("");
    lines.push(`- 提示词：${run.prompt}`);
    lines.push(`- 验证（修复前后）：${run.preCheck} → ${run.postCheck}`);
    if (run.metrics) {
      lines.push(`- 工具调用序列：${run.metrics.toolCalls.map((c) => `${c.toolName}(${c.durationMs ?? "?"}ms${c.isError ? ", error" : ""})`).join(" → ") || "（无）"}`);
      lines.push(`- turn 用时：${run.metrics.turnSpans.map((s) => `t${s.turn}=${s.ms}ms`).join(", ") || "—"}`);
      if (run.metrics.reasoningTokens) lines.push(`- thinking tokens：${run.metrics.reasoningTokens}`);
      if (run.metrics.unparsedLines) lines.push(`- 非 JSON 行：${run.metrics.unparsedLines}（见 artifacts）`);
      if (run.metrics.timedOut) lines.push(`- **超时终止**（${config.timeoutMs}ms）`);
    }
    lines.push(`- 变更文件：${run.changed.length ? run.changed.join(", ") : "（无）"}`);
    for (const file of run.changed) {
      lines.push("");
      lines.push(`### ${file}（运行后）`);
      lines.push("```");
      lines.push(run.after[file] === null ? "（文件缺失）" : run.after[file].replace(/\n$/, ""));
      lines.push("```");
    }
    lines.push("");
  }
  lines.push("## Artifacts");
  lines.push("");
  for (const run of runs) {
    lines.push(`- ${run.label}/${run.task}: \`${run.eventsPath ?? "-"}\`, \`${run.stderrPath ?? "-"}\`, \`${run.verifyPath ?? "-"}\`, \`${run.workDir ?? "-"}\``);
  }
  lines.push("");
  writeFileSync(join(outDir, "report.md"), lines.join("\n"));
  return lines.join("\n");
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    process.stdout.write(`${USAGE}\n`);
    return 0;
  }
  const outDir = resolve(args.out);
  const tasks = args.task === "both" ? ["bug", "feature"] : [args.task];

  // Dry run: fixture pre-checks only (local `node --test`, no binary, no key,
  // no network, no agent dir). This works on a machine where neither CLI is
  // installed — the old path resolved/hashed binaries before looking at
  // --dry-run and failed there.
  if (args.dryRun) {
    prepareOutDir(outDir, args.force);
    const runs = [];
    let fixtureBroken = false;
    for (const task of tasks) {
      const fixtureRoot = mkdtempSync(join(tmpdir(), `pir-p3-fixture-${task}-`));
      const fixtureDir = materializeFixture(fixtureRoot, task);
      try {
        const pre = await runFixtureTests(fixtureDir);
        const preCheck = pre.status === 0 ? "PASS(!! fixture already passes)" : `FAIL(exit ${pre.status})`;
        if (pre.status === 0) fixtureBroken = true;
        const targets = args.noPi ? ["pir"] : ["pir", "pi"];
        for (const label of targets) {
          runs.push({
            label, task, prompt: FIXTURES[task].prompt, preCheck, postCheck: "dry-run",
            verdict: "DRY", changed: [], after: {}, metrics: null,
            eventsPath: null, stderrPath: null, verifyPath: null,
          });
        }
        process.stdout.write(`[p3] dry-run · ${task}: ${preCheck}\n`);
      } finally {
        rmSync(fixtureRoot, { recursive: true, force: true });
      }
    }
    process.stdout.write(`[p3] dry-run: no binary resolved, no key read, no model call\n`);
    process.stdout.write(`[p3] pir=${args.cli} pi=${args.noPi ? "(disabled)" : args.piCli} model=${args.model}\n`);
    writeReport(outDir, runs, { model: args.model, dryRun: true, timeoutMs: args.timeoutMs });
    writeFileSync(join(outDir, "report.json"), `${JSON.stringify({ dryRun: true, model: args.model, tasks, runs }, null, 2)}\n`);
    process.stdout.write(`[p3] report: ${join(outDir, "report.md")}\n`);
    return fixtureBroken ? 3 : 0;
  }

  args.cli = resolve(args.cli);
  if (!existsSync(args.cli)) throw new Error(`pir binary not found: ${args.cli}`);
  if (!args.noPi) {
    // Fail the CAMPAIGN, not every leg: a bare/unresolvable pi CLI otherwise
    // records N legs as "FAIL · wall 3ms · turns 0" (measured: a campaign
    // launched without --pi-cli used PATH's Unix shim, which Node cannot
    // execute on Windows), which reads like model verdicts.
    const pi = args.piCli;
    const looksLikePath = pi.includes("/") || pi.includes("\\") || /\.(cmd|bat|exe)$/i.test(pi);
    if (looksLikePath && !existsSync(pi)) {
      throw new Error(`pi CLI not found: ${pi} (pass --pi-cli <pi.cmd>)`);
    }
  }
  const binaries = {
    pir: binaryInfo("pir", args.cli),
    pi: args.noPi ? null : binaryInfo("pi", args.piCli),
  };
  if (!args.noPi) assertDistinctBinaries(binaries.pir, binaries.pi);
  // A stale release build makes the comparison meaningless; refuse it unless
  // the caller explicitly freezes the binary.
  const freshness = checkReleaseFreshness(binaries.pir, args.allowStale);
  // Only now — after every validation — touch the artifacts dir, so a refused
  // invocation leaves no empty directory behind.
  prepareOutDir(outDir, args.force);

  // Per-LEG isolated agent dirs: a temp copy of the real config (credentials
  // never printed). Both CLIs honor PI_CODING_AGENT_DIR; pir also honors
  // PIR_AGENT_DIR. One shared dir would let the first leg's writes leak into
  // the second leg.
  const realAgentDir = args.agentDir
    ? resolve(args.agentDir)
    : join(process.env.USERPROFILE ?? process.env.HOME ?? "", ".pi", "agent");
  if (args.agentDir && !existsSync(realAgentDir)) throw new Error(`--agent-dir not found: ${realAgentDir}`);
  const modelsJson = join(realAgentDir, "models.json");
  if (!existsSync(modelsJson)) throw new Error(`models.json not found at ${modelsJson}`);
  const runs = [];
  const agentDirs = {};
  const makeLegAgentDir = (label) => {
    const dir = mkdtempSync(join(tmpdir(), `pir-p3-agent-${label}-`));
    // The isolated agent dir needs the WHOLE config pair, not just models.json:
    // settings.json carries defaultProvider/defaultModel (without it the CLI
    // resolved the first provider in models.json instead of the requested one).
    for (const name of ["models.json", "settings.json", "auth.json"]) {
      const src = join(realAgentDir, name);
      if (existsSync(src)) writeFileSync(join(dir, name), readFileSync(src));
    }
    agentDirs[label] = dir;
    return dir;
  };
  try {
    const childEnvFor = (label) => {
      const dir = makeLegAgentDir(label);
      return { ...process.env, PIR_AGENT_DIR: dir, PI_CODING_AGENT_DIR: dir };
    };

    for (const task of tasks) {
      const fixtureRoot = mkdtempSync(join(tmpdir(), `pir-p3-fixture-${task}-`));
      const fixtureDir = materializeFixture(fixtureRoot, task);
      const prompt = FIXTURES[task].prompt;

      // Fixture sanity: the task must START failing.
      const pre = await runFixtureTests(fixtureDir);
      const preCheck = pre.status === 0 ? "PASS(!! fixture already passes)" : `FAIL(exit ${pre.status})`;
      if (pre.status === 0) {
        rmSync(fixtureRoot, { recursive: true, force: true });
        throw new Error(`fixture ${task} passes before the task; it must fail to be a real task`);
      }

      const before = snapshot(fixtureDir, task);
      rmSync(fixtureRoot, { recursive: true, force: true });

      // `--runs N` really means N independent repetitions: expand the CLI
      // list per repetition (labels become `pir#2`, `pi#2`, …) instead of
      // silently running each CLI once.
      const reps = Math.max(1, args.runs);
      const clis = [];
      for (let rep = 1; rep <= reps; rep += 1) {
        const suffix = reps > 1 ? `#${rep}` : "";
        clis.push({ cli: args.cli, label: `pir${suffix}` });
        if (!args.noPi) clis.push({ cli: args.piCli, label: `pi${suffix}` });
      }

      for (const target of clis) {
        const runOut = join(outDir, `artifacts-${target.label}-${task}`);
        mkdirSync(runOut, { recursive: true });
        // Fresh fixture copy per CLI, outside the repo and outside outDir, so
        // the agent cannot discover the harness or prior run artifacts.
        const runDir = mkdtempSync(join(tmpdir(), `pir-p3-${task}-${target.label}-`));
        const keepDir = join(outDir, "work", `${task}-${target.label}`);
        for (const [file, content] of Object.entries(before)) {
          if (content !== null) writeFileSync(join(runDir, file), content);
        }
        const preRun = await runFixtureTests(runDir);
        if (preRun.status === 0) {
          rmSync(runDir, { recursive: true, force: true });
          throw new Error(`run fixture ${runDir} passes before the task`);
        }

        process.stdout.write(`[p3] ${target.label} · ${task} · model ${args.model} …\n`);
        const hand = runTask({
          cli: target.cli,
          label: target.label,
          fixtureDir: runDir,
          prompt,
          model: args.model,
          timeoutMs: args.timeoutMs,
          outDir: runOut,
          dryRun: false,
          // Fresh agent dir for THIS leg only: the two legs never share one.
          childEnv: childEnvFor(target.label),
        });
        const closeInfo = await new Promise((resolveClose) => {
          hand.done.then(resolveClose);
          const poll = setInterval(() => {
            if (hand.child.exitCode !== null || hand.child.signalCode !== null) {
              clearInterval(poll);
              resolveClose({ code: hand.child.exitCode });
            }
          }, 250);
        });
        hand.clearTimer();
        const wallMs = Date.now() - hand.started;
        const metrics = summarize(
          hand.events,
          hand.started,
          closeInfo,
          hand.firstLineAt,
          hand.requestAt,
          hand.ttftAt,
          hand.killedByTimeout,
        );
        metrics.wallMs = wallMs;
        metrics.reasoningTokens = metrics.usage.reasoning;
        metrics.peakRssMb = Math.round((hand.rss?.peakMb ?? 0) * 10) / 10;
        writeFileSync(hand.eventsPath, hand.events.map((e) => JSON.stringify(e)).join("\n"));
        writeFileSync(hand.stderrPath, hand.stderrChunks.join(""));

        let post;
        let after;
        let changed;
        let pristine;
        try {
          // The prompt says "do not change the test file". Enforce it: run the
          // fixture's pristine tests over a copy of the agent's source tree.
          pristine = testsArePristine(runDir, task);
          post = await runPristineFixtureTests(runDir, task);
          after = snapshot(runDir, task);
          changed = Object.keys(before).filter((file) => before[file] !== after[file]);
        } finally {
          archiveDir(runDir, keepDir);
          rmSync(runDir, { recursive: true, force: true });
        }
        const verifyPath = join(runOut, `verify-${target.label}-${task}.log`);
        writeFileSync(verifyPath, `# exit ${post.status}\n# pristine tests: ${pristine ? "yes" : "NO (test file modified)"}\n${post.stdout}\n${post.stderr}`);
        const verdict = !metrics.timedOut && pristine && post.status === 0 ? "PASS" : "FAIL";
        if (metrics.spawnError) metrics.verdictNote = `spawn error: ${metrics.spawnError}`;
        if (metrics.timedOut) metrics.verdictNote = "timed out";
        runs.push({
          label: target.label,
          task,
          prompt,
          preCheck: `FAIL(exit ${preRun.status})`,
          postCheck: `${verdict}(exit ${post.status}${pristine ? "" : ", test file modified"}${metrics.timedOut ? ", timed out" : ""})`,
          verdict,
          changed,
          after,
          metrics,
          eventsPath: hand.eventsPath,
          stderrPath: hand.stderrPath,
          verifyPath,
          workDir: keepDir,
        });
        process.stdout.write(
          `[p3] ${target.label} · ${task} → ${verdict} · wall ${wallMs}ms · turns ${metrics.turns} · tools ${metrics.toolCalls.length} · tokens ${metrics.usage.totalTokens}\n`,
        );
      }
    }
  } finally {
    // Every leg has its own agent dir; each holds a credentials copy, so all
    // of them are removed unless --keep-agent-dir was explicitly passed.
    for (const [label, dir] of Object.entries(agentDirs)) {
      if (!args.keepAgentDir) {
        rmSync(dir, { recursive: true, force: true });
      } else {
        process.stdout.write(`[p3] kept ${label} agent dir (contains credentials): ${dir}\n`);
      }
    }
  }

  const report = writeReport(outDir, runs, {
    model: args.model,
    dryRun: args.dryRun,
    timeoutMs: args.timeoutMs,
  });
  process.stdout.write(`\n[p3] report: ${join(outDir, "report.md")}\n`);
  process.stdout.write(report.split("\n").slice(0, 12).join("\n") + "\n");

  // Machine-readable summary: per-leg medians over stall-free runs, plus the
  // verdict and the fixture pre-check. Written beside report.md.
  const legs = {};
  for (const run of runs) {
    // Multi-repetition labels are `pir#2`/`pi#2`; group them under the base
    // leg so the medians are across repetitions, not across one run each.
    const label = String(run.label).replace(/#\d+$/, "");
    const m = run.metrics;
    const stallFree = Boolean(m) && !m.timedOut && !m.spawnError
      && Number.isFinite(m.ttftFromRequestMs ?? m.ttftMs)
      && (m.ttftFromRequestMs ?? m.ttftMs) <= args.stallTtftMs;
    const mine = (legs[label] ??= { runs: 0, pass: 0, fail: 0, kept: 0, dropped: 0, samples: [] });
    mine.runs += 1;
    if (run.verdict === "PASS") mine.pass += 1;
    else mine.fail += 1;
    if (stallFree) {
      mine.kept += 1;
      mine.samples.push(m);
    } else {
      mine.dropped += 1;
    }
  }
  for (const leg of Object.values(legs)) {
    const pick = (fn) => median(leg.samples.map(fn));
    leg.median = {
      wallMs: pick((m) => m.wallMs),
      startupMs: pick((m) => m.startupMs),
      ttftMs: pick((m) => m.ttftMs),
      ttftFromRequestMs: pick((m) => m.ttftFromRequestMs),
      peakRssMb: pick((m) => m.peakRssMb),
      turns: pick((m) => m.turns),
      totalTokens: pick((m) => m.usage?.totalTokens),
    };
    leg.rankable = leg.kept > 0 && leg.fail === 0;
    delete leg.samples;
  }
  const machineReport = {
    generatedAt: new Date().toISOString(),
    kind: "p3-fixture-quality",
    task: "p3",
    qualityScore: true,
    model: args.model,
    outDir: outDir.replace(/\\/g, "/"),
    binaries,
    freshness,
    stallRule: { ttftMs: args.stallTtftMs },
    rankingRule: "medians over stall-free runs only; verdict first (pristine fixture tests)",
    legs,
  };
  writeFileSync(join(outDir, "report.json"), `${JSON.stringify(machineReport, null, 2)}\n`);
  // summary.json is the file a campaign consumes; every harness in scripts/perf
  // writes one with the same meaning (per-leg stall-free medians + verdict).
  writeFileSync(join(outDir, "summary.json"), `${JSON.stringify(machineReport, null, 2)}\n`);

  const allPass = runs.length > 0 && runs.every((run) => run.verdict === "PASS" || run.verdict === "DRY");
  const spawnFailed = runs.some((run) => run.metrics?.spawnError);
  // 3 = task verdict FAIL; 2 = spawn/harness error; 0 = all pass.
  process.exitCode = spawnFailed ? 2 : (allPass ? 0 : 3);
  return process.exitCode;
}

main().then((code) => {
  if (typeof code === "number") process.exitCode = code;
}).catch((error) => {
  const message = String(error?.message ?? error);
  if (/unknown flag|needs a value|bad --|must be|not found|no quality score/i.test(message)) {
    process.stderr.write(`[p3] ${message}\n`);
    process.exitCode = 1;
    return;
  }
  process.stderr.write(`[p3] error: ${error.stack ?? error}\n`);
  process.exitCode = 2;
});
