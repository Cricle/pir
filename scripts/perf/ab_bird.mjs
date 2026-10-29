#!/usr/bin/env node
/**
 * Fair A/B: `pir` vs upstream `pi` on one identical prompt — build a playable
 * "Angry Birds" web game — with comprehensive per-run metrics and an artifact
 * verdict.
 *
 * Fairness controls (all printed into the report):
 *  - ONE prompt string, used verbatim for both CLIs (argv, not stdin);
 *  - same model + gateway; runs are SEQUENTIAL and interleaved per repetition
 *    (pir, pi, pir, pi …) so gateway drift hits both;
 *  - both CLIs get their own EMPTY work dir and their own isolated agent dir
 *    (temp copy of models.json/settings.json/auth.json; deleted afterwards), so
 *    neither reads the user's sessions/extensions/caches;
 *  - identical argv shape (`--mode json --print --model M <prompt>`), stdin
 *    closed (a non-TTY stdin is read as piped input and would block), same
 *    timeout, same RSS sampler (tasklist every 500 ms);
 *  - the verdict is the produced artifact set, judged by the same function for
 *    both CLIs: index.html exists and is non-trivial, and every .js parses.
 *
 * SCOPE / HONESTY: `bird` and `app` are ARTIFACT checks, not hidden-test
 * quality scores. `bird` verifies the produced web-game tree has the required
 * shape and parses; `app` runs the project's OWN test suite (self-eval, so a
 * weak suite earns a PASS). They are useful A/B speed/cost and
 * "did-it-produce-anything-sane" signals only. For quality use
 * `scripts/perf/harder_tasks_campaign.mjs` (hidden-test scores) or
 * `scripts/perf/p3_task_e2e.mjs` (pristine fixture tests), or run
 * `scripts/perf/ab-ar/run_ab.mjs --task quality`, which routes there.
 * Every report states this; `report.json` carries `checkKind`.
 *
 * Usage:
 *   node scripts/perf/ab_bird.mjs --pir <pir.exe> --pi-cli <pi.cmd> [--runs 2]
 *       [--model openai/gpt-5] [--timeout-ms 900000] [--out DIR] [--force]
 *   node scripts/perf/ab_bird.mjs --task app --judge <artifact-dir>
 *       (re-score an existing artifact set with the same verdict, no CLI run)
 *   node scripts/perf/ab_bird.mjs --dry-run [...]
 *       (print the plan; resolves no binary, creates no dir, needs no key)
 *   node scripts/perf/ab_bird.mjs --help
 *
 * Standalone fairness: each leg gets its OWN freshly created agent dir (never
 * one shared dir), so no session/cache/config state can pass between them.
 * `ab-ar/run_ab.mjs` remains the fully isolated path (throwaway HOME plus
 * persistent per-leg launcher dirs). When `PIR_AB_PIR_BIN`/`PIR_AB_PI_BIN` are
 * set (run_ab does), the identity hash MUST match the binary the leg actually
 * launches or the run is refused.
 *
 * Exit code 0 when every run's artifact verdict passed, 3 when a verdict
 * failed, 2 on a harness error.
 */

import { execFile, spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");

const TASKS = {
  bird: {
    prompt:
      "Build a playable Angry Birds web game in this directory. Requirements: " +
      "(1) a single index.html that runs by double-clicking it: no build step, no local server, " +
      "no network access and no external dependencies; " +
      "(2) canvas-based rendering with simple 2D physics: drag the slingshot to aim and release to " +
      "launch, gravity, collisions, and destructible blocks; " +
      "(3) at least 3 levels with targets (pigs), a score/HUD, level switching, and win/lose states " +
      "with restart; " +
      "(4) a few well-organized files (index.html plus one or more .js and .css files) and a short " +
      "README.md explaining how to play; " +
      "(5) it must run in a modern browser without console errors. " +
      "When you are done, re-read the files you wrote to verify they are complete.",
    verdict: "artifacts",
  },
  // Request-level probe: one delta-free-of-tools turn, so wall clock is
  // startup + exactly one provider round trip. Separates "request latency &
  // throughput" from "agent behaviour on a long task".
  probe: {
    prompt: "Reply with exactly this text and nothing else: OK. Do not use any tools.",
    verdict: "probe",
  },
  // Streaming-throughput probe: one long, deterministic generation. Wall minus
  // startup/TTFT over the output tokens is the effective token rate, so a
  // consumer-side backpressure throttle (bounded event queue) shows up here.
  gen: {
    prompt:
      "Output the integers from 1 to 500 on a single line, separated by single spaces. " +
      "Do not use any tools. Output nothing else.",
    verdict: "probe",
  },
  // A REAL application task with an objective verdict: the produced project
  // must pass its own `node --test` suite (stdlib only, offline).
  app: {
    prompt:
      "Build a real, working application in this directory: a bookmark-manager REST API in " +
      "Node.js using ONLY the Node standard library (no npm install, no network at runtime). " +
      "Requirements: (1) `node server.js` starts an HTTP server on process.env.PORT (default " +
      "3000) with a JSON REST API: GET /api/bookmarks (list, with ?tag=, ?q=, ?page=, ?perPage= " +
      "filters and a total count), POST /api/bookmarks (create), GET/PUT/DELETE " +
      "/api/bookmarks/:id; a bookmark has id, url, title, tags[], createdAt, updatedAt. " +
      "(2) data persists to a JSON file in this directory and survives a server restart. " +
      "(3) validation with proper status codes (400 invalid input, 404 unknown id, 409 duplicate " +
      "url) and one consistent JSON error shape {\"error\": {\"code\", \"message\"}}. " +
      "(4) a minimal static UI served at / (one HTML file plus vanilla JS) that lists, creates " +
      "and deletes bookmarks through the API. " +
      "(5) a test suite using the built-in `node:test` + `node:assert` (no dependencies) covering " +
      "CRUD, validation, filtering/pagination and persistence across a restart, runnable with " +
      "`node --test` from this directory. " +
      "(6) a README.md explaining how to run the server and the tests. " +
      "Run `node --test` yourself and make the whole suite pass before you finish.",
    verdict: "node-app",
  },
};
const PROMPT = TASKS.bird.prompt;

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

const USAGE = [
  "Usage: node scripts/perf/ab_bird.mjs --pir <exe> --pi-cli <cmd> [flags]",
  "",
  "  --task bird|app|probe|gen  task (default bird). bird/app are ARTIFACT",
  "                             checks; use harder_tasks_campaign.mjs or",
  "                             p3_task_e2e.mjs for quality scores.",
  "  --quality                  refuse and point at the quality harness",
  "                             (use ab-ar/run_ab.mjs --task quality)",
  "  --pir <exe>                pir binary (default target-alt/release/pir.exe)",
  "  --pi-cli <cmd>             upstream pi entry point (default %APPDATA%/npm/pi.cmd)",
  "  --model <provider/id>      model id (default openai/gpt-5)",
  "  --runs N                   repetitions; legs interleave pir,pi,pir,pi (default 1)",
  "  --timeout-ms N             per-run timeout (default 900000)",
  "  --out <dir>                artifacts dir (outside the repo; empty unless --force)",
  "  --thinking <level>         pinned thinking level (default low)",
  "  --no-skills                pass --no-skills to both legs (fair prefixes)",
  "  --prompt-file <path>       override the task prompt (both legs verbatim)",
  "  --judge <dir>              re-score an existing artifact dir, no CLI run",
  "  --force                    replace a non-empty --out dir",
  "  --allow-stale              accept a release pir older than src/ (frozen build)",
  "  --stall-wall-ms N          dirty wall threshold for probe/gen runs (default 8000)",
  "  --stall-ttft-ms N          dirty TTFT threshold for every task (default 10000)",
  "  --legs both|pir|pi         which legs to run (default both). A probe may",
  "                             run one leg per repetition; verdicts are per leg.",
  "  --agent-dir <dir>          source agent config dir (default ~/.pi/agent);",
  "                             each leg still gets its own fresh copy",
  "  --dry-run                  print the plan and exit; resolves no binary,",
  "                             needs no key/network, creates nothing",
  "  -h, --help                 this text",
  "",
  "Exit: 0 all verdicts PASS · 3 verdict FAIL · 2 harness error.",
].join("\n");

function parseArgs(argv) {
  const args = {
    pir: join(REPO, "target-alt", "release", "pir.exe"),
    piCli: join(process.env.APPDATA ?? "", "npm", "pi.cmd"),
    model: "openai/gpt-5",
    runs: 1,
    timeoutMs: 900_000,
    // A per-process default: two concurrent standalone runs must never share
    // (and --force-wipe) one artifacts dir.
    out: join(tmpdir(), "pir-ab-bird", `${new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19)}-${process.pid}`),
    task: "bird",
    promptFile: null,
    thinking: "low",
    noSkills: false,
    // Equalized-surface control for A/B fairness: both legs receive the same
    // `--tools <list>`; unset means each CLI's own default loadout.
    tools: null,
    force: false,
    dryRun: false,
    help: false,
    allowStale: false,
    stallWallMs: 8_000,
    stallTtftMs: 10_000,
    legs: "both",
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
      case "--pir": args.pir = next(); break;
      case "--pi-cli": args.piCli = next(); break;
      case "--model": args.model = next(); break;
      case "--runs": args.runs = Number.parseInt(next(), 10); break;
      case "--timeout-ms": args.timeoutMs = Number.parseInt(next(), 10); break;
      case "--out": args.out = next(); break;
      case "--task": args.task = next(); break;
      case "--prompt-file": args.promptFile = next(); break;
      case "--thinking": args.thinking = next(); break;
      case "--no-skills": args.noSkills = true; break;
      case "--tools": args.tools = next(); break;
      case "--force": args.force = true; break;
      case "--judge": args.judge = next(); break;
      case "--dry-run": args.dryRun = true; break;
      case "--allow-stale": args.allowStale = true; break;
      case "--stall-wall-ms": args.stallWallMs = Number.parseInt(next(), 10); break;
      case "--stall-ttft-ms": args.stallTtftMs = Number.parseInt(next(), 10); break;
      case "--legs": args.legs = next(); break;
      case "--agent-dir": args.agentDir = next(); break;
      case "--quality":
        throw new Error(
          "ab_bird has no quality score: `bird`/`app` are artifact checks. " +
          "Use `node scripts/perf/harder_tasks_campaign.mjs` (hidden tests), " +
          "`node scripts/perf/p3_task_e2e.mjs` (pristine fixture tests), or " +
          "`node scripts/perf/ab-ar/run_ab.mjs --task quality`.",
        );
      case "-h":
      case "--help": args.help = true; break;
      default: throw new Error(`unknown flag ${flag} (see --help)`);
    }
  }
  if (!TASKS[args.task]) throw new Error(`unknown task ${args.task} (have: ${Object.keys(TASKS).join(", ")})`);
  if (!Number.isInteger(args.runs) || args.runs < 1) throw new Error(`--runs must be a positive integer, got ${args.runs}`);
  if (!Number.isInteger(args.timeoutMs) || args.timeoutMs < 1_000) throw new Error(`--timeout-ms must be >= 1000, got ${args.timeoutMs}`);
  if (!Number.isInteger(args.stallWallMs) || args.stallWallMs < 1) throw new Error(`--stall-wall-ms must be a positive integer, got ${args.stallWallMs}`);
  if (!Number.isInteger(args.stallTtftMs) || args.stallTtftMs < 1) throw new Error(`--stall-ttft-ms must be a positive integer, got ${args.stallTtftMs}`);
  if (!["both", "pir", "pi"].includes(args.legs)) throw new Error(`--legs must be both|pir|pi, got ${args.legs}`);
  return args;
}

// ---------------------------------------------------------------------------
// Artifact collection + verdict (identical for both CLIs)
// ---------------------------------------------------------------------------

const SKIP_DIRS = new Set(["node_modules", ".git", "target-alt", "target"]);

function walkFiles(root, dir = root, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name.startsWith(".") && entry.name !== ".gitignore") continue;
    const abs = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (SKIP_DIRS.has(entry.name)) continue;
      walkFiles(root, abs, out);
    } else if (entry.isFile()) {
      out.push(relative(root, abs).replace(/\\/g, "/"));
    }
  }
  return out.sort();
}

function collectArtifacts(dir) {
  const files = walkFiles(dir);
  const rows = [];
  let bytes = 0;
  let lines = 0;
  for (const rel of files) {
    const abs = join(dir, rel);
    const size = statSync(abs).size;
    const text = readFileSync(abs, "utf8");
    const lineCount = text.split("\n").length;
    bytes += size;
    lines += lineCount;
    rows.push({ path: rel, bytes: size, lines: lineCount });
  }
  return { files, rows, bytes, lines };
}


/**
 * `node --test`'s summary line, in either reporter format: TAP (`# pass 14`)
 * or the spec reporter (`ℹ pass 14`, Node's default when stdout is a pipe).
 * Parsing only TAP read a passing suite as 0 passes — measured: both legs of
 * the first app run were reported FAIL ("only 0 passing tests") while their
 * suites actually passed.
 */
function parseTestSummary(output) {
  const num = (re) => Number((re.exec(output) ?? [])[1] ?? 0);
  return {
    pass: num(/^(?:#|ℹ)\s*pass (\d+)/m),
    fail: num(/^(?:#|ℹ)\s*fail (\d+)/m),
  };
}

/**
 * Bounded `node --test` for the app verdict. `spawnSync({timeout})` cannot kill
 * a process TREE on Windows: a server the fixture started survives the timeout
 * holding the inherited stdio pipes, so spawnSync waits FOREVER — measured: the
 * first app run wedged inside its own verdict with `node --test` still alive 10
 * min past a 180 s timeout, blocking the whole A/B run. So: async spawn, an
 * explicit per-test timeout (a hung test fails instead of hanging the suite),
 * and a tree-kill on our deadline that also unblocks stdio.
 */
function runNodeTest(dir, timeoutMs = 180_000, perTestMs = 30_000) {
  return new Promise((resolve) => {
    const child = spawn("node", ["--test", "--test-reporter=tap", `--test-timeout=${perTestMs}`], {
      cwd: dir, windowsHide: true, stdio: ["ignore", "pipe", "pipe"],
    });
    let out = "";
    let err = "";
    let timedOut = false;
    let settled = false;
    let timer = null;
    let killTimer = null;
    let exitTimer = null;
    const destroyPipes = () => {
      try { child.stdout?.destroy(); } catch {}
      try { child.stderr?.destroy(); } catch {}
    };
    const finish = (status) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearTimeout(killTimer);
      clearTimeout(exitTimer);
      resolve({ status, out, err, timedOut });
    };
    child.stdout?.on("data", (chunk) => { out += chunk; });
    child.stderr?.on("data", (chunk) => { err += chunk; });
    timer = setTimeout(() => {
      timedOut = true;
      if (child.pid !== undefined) {
        spawnSync("taskkill", ["/F", "/T", "/PID", String(child.pid)], {
          windowsHide: true, timeout: 30_000,
        });
      }
      // `taskkill /T` can miss a descendant that inherited the stdio pipes
      // (MSYS hop, or the root dies before the sweep), in which case `close`
      // never fires. Bound the wait: close OUR pipe ends and settle 124.
      killTimer = setTimeout(() => {
        destroyPipes();
        finish(124);
      }, 5000);
    }, timeoutMs);
    // Mirror runOne: after the root exits, a survivor holding the inherited
    // pipes must not keep `close` from ever firing.
    child.on("exit", () => {
      exitTimer = setTimeout(destroyPipes, 5000);
    });
    child.on("error", () => finish(null));
    child.on("close", (code) => finish(code));
  });
}

/** Verdict for the `app` task: the produced project passes its own `node --test`. */
async function nodeAppVerdict(dir) {
  const { files, bytes } = collectArtifacts(dir);
  const notes = [];
  const jsFiles = files.filter((f) => /\.(js|mjs)$/i.test(f));
  if (jsFiles.length === 0) notes.push("no .js files");
  for (const rel of jsFiles) {
    const checked = spawnSync("node", ["--check", rel], { cwd: dir, encoding: "utf8", windowsHide: true });
    if (checked.status !== 0) {
      const first = String(checked.stderr ?? "").split(/\r?\n/)[0].trim();
      notes.push(`${rel} fails node --check: ${first}`);
    }
  }
  if (bytes < 8192) notes.push(`the artifact set is only ${bytes} B (< 8 KB)`);
  if (!files.some((f) => /readme\.md$/i.test(f))) notes.push("no README.md");
  if (!files.some((f) => /(^|\/)(index\.html|public\/.*\.html)$/i.test(f))) notes.push("no static UI HTML");
  const tests = await runNodeTest(dir);
  const output = `${tests.out}\n${tests.err}`;
  const { pass, fail } = parseTestSummary(output);
  if (tests.timedOut) notes.push(`node --test exceeded its budget and was tree-killed (pass=${pass} fail=${fail})`);
  else if (tests.status !== 0) notes.push(`node --test exit ${tests.status} (pass=${pass} fail=${fail})`);
  if (pass < 4) notes.push(`only ${pass} passing tests (< 4)`);
  return { ok: notes.length === 0, notes, tests: { pass, fail, status: tests.status ?? null, timedOut: tests.timedOut } };
}

/** Same verdict for both CLIs: a runnable artifact set. */
function artifactVerdict(dir) {
  const { files, bytes } = collectArtifacts(dir);
  const notes = [];
  const htmlRel = files.find((f) => /(^|\/)index\.html$/i.test(f));
  if (!htmlRel) {
    return { ok: false, notes: [`no index.html (found: ${files.join(", ") || "nothing"})`] };
  }
  const htmlDir = dirname(join(dir, htmlRel));
  const html = readFileSync(join(dir, htmlRel), "utf8");
  // A thin index.html that loads external scripts/styles is the requested
  // layout ("index.html plus one or more .js and .css files") — the size check
  // is on the whole artifact set, not on the shell.
  if (!/<canvas/i.test(html)) notes.push("index.html has no <canvas> element");
  if (!/<script/i.test(html)) notes.push("index.html loads no script");
  const scriptSrcs = [...html.matchAll(/<script[^>]*\bsrc\s*=\s*["']([^"']+)["']/gi)].map((m) => m[1]);
  for (const src of scriptSrcs) {
    if (/^[a-z]+:\/\//i.test(src)) {
      notes.push(`index.html loads a remote script ${src}`);
      continue;
    }
    const rel = src.replace(/^\.\//, "").split("?")[0];
    if (!existsSync(join(htmlDir, rel))) notes.push(`index.html references a missing script ${src}`);
  }
  const jsFiles = files.filter((f) => /\.(js|mjs)$/i.test(f));
  for (const rel of jsFiles) {
    const checked = spawnSync("node", ["--check", rel], { cwd: dir, encoding: "utf8", windowsHide: true });
    if (checked.status !== 0) {
      notes.push(`${rel} fails node --check: ${String(checked.stderr ?? "").split("\n")[0].trim()}`);
    }
  }
  if (jsFiles.length === 0) {
    const inline = /<script[^>]*>([\s\S]*?)<\/script>/i.exec(html);
    if (!inline || inline[1].trim().length < 500) {
      notes.push("no .js file and no substantial inline script body");
    }
  }
  if (bytes < 8192) notes.push(`the artifact set is only ${bytes} B (< 8 KB)`);
  if (!files.some((f) => /readme\.md$/i.test(f))) notes.push("no README.md");
  return { ok: notes.length === 0, notes };
}

// ---------------------------------------------------------------------------
// One run
// ---------------------------------------------------------------------------

/**
 * Peak working set of the WHOLE PROCESS TREE, in MB. `pi.cmd` is a cmd.exe
 * shim, so sampling the spawned pid alone measures the shim (measured: 10 MB)
 * instead of node; the tree sum is the fair instrument for both CLIs and also
 * covers tool subprocesses (bash etc.).
 */
function samplePeakTreeRssMb(child, state) {
  if (process.platform !== "win32") return;
  const script =
    "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ProcessId) $($_.ParentProcessId) $($_.WorkingSetSize)\" }";
  // ASYNC on purpose: the earlier spawnSync sampler blocked the event loop for
  // seconds per sample under load, which starved the run timeout's timer (a leg
  // was measured hanging 29 min past a 20-min timeout). execFile keeps the loop
  // free so `setTimeout` and the child's stdio events keep firing.
  state.timer = setInterval(() => {
    execFile("powershell", ["-NoProfile", "-Command", script], { windowsHide: true, timeout: 5000 },
      (error, stdout) => {
        if (error || !stdout) return;
        const rows = String(stdout)
          .trim()
          .split(/\r?\n/)
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
        let totalBytes = own.get(child.pid) ?? 0;
        const stack = [child.pid];
        // Guard the parent/child walk against a CIM cycle: without it the
        // stack can grow without bound (`RangeError: Invalid array length`).
        const visited = new Set();
        while (stack.length) {
          const cur = stack.pop();
          if (visited.has(cur)) continue;
          visited.add(cur);
          for (const kid of children.get(cur) ?? []) {
            totalBytes += own.get(kid) ?? 0;
            stack.push(kid);
          }
        }
        const mb = totalBytes / (1024 * 1024);
        if (Number.isFinite(mb)) state.peakMb = Math.max(state.peakMb, mb);
      });
  }, 3000);
}

/**
 * Kill a run's whole process tree.
 *
 * `taskkill /T` follows Windows parent links, but an MSYS bash hop can orphan
 * its node grandchild (pir.exe → bash.exe → node). That grandchild keeps the
 * inherited stdio pipes open, so `child.on("close")` never fires and the whole
 * A/B run wedges — measured: after a 900 s timeout the run had not reported
 * 25 minutes later, with a surviving `node --test` in the run's directory. So:
 * taskkill the tree, then stop every process whose command line names THIS
 * run's unique working directory or whose executable lives inside it.
 *
 * CWD-only descendants: `Get-CimInstance Win32_Process` exposes no working
 * directory, and `node --test` is started with `cwd: dir` but resolves node.exe
 * from PATH, so it carries neither marker. Such a process is covered only by
 * the recursive parent walk (and by the bounded pipe-destroy + finish(124)
 * fallback in runNodeTest). That is the documented limit of this fallback.
 */
function killRunTree(child, dir) {
  if (child.pid !== undefined) {
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
      windowsHide: true, timeout: 30_000,
    });
  }
  if (process.platform !== "win32") {
    try { child.kill("SIGKILL"); } catch {}
    return;
  }
  const quoted = dir.replace(/'/g, "''");
  // Trailing separator avoids prefix collisions (`...-r1` vs `...-r10`).
  const quotedChild = `${dir}\\`.replace(/'/g, "''");
  // 1) Fixed-point WINDOWS-PARENT walk from the agent pid (taskkill /T can miss
  //    MSYS-descended grandchildren once the root is dying). The loop also
  //    walks descendants of the dir-matched processes found in (2)/(3).
  // 2) Any process whose command line names THIS run's unique directory.
  // 3) Any process whose executable lives under THIS run's directory (e.g. a
  //    bundled node.exe the fixture copied there); CIM cannot expose the
  //    process working directory, so a plain `cwd: dir` spawn is not matched.
  const script =
    `$root=${child.pid ?? 0}; $me=$PID; ` +
    `$all=Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,CommandLine,ExecutablePath; ` +
    `$ids=New-Object 'System.Collections.Generic.HashSet[int]'; [void]$ids.Add($root); ` +
    `for ($i=0; $i -lt 12; $i++) { $changed=$false; foreach ($p in $all) { ` +
    `$id=[int]$p.ProcessId; if ($ids.Contains($id)) { continue }; ` +
    `$hit=$ids.Contains([int]$p.ParentProcessId); ` +
    `if (-not $hit -and $p.CommandLine) { $hit=$p.CommandLine.Contains('${quoted}') }; ` +
    `if (-not $hit -and $p.ExecutablePath) { $hit=$p.ExecutablePath.StartsWith('${quotedChild}',[System.StringComparison]::OrdinalIgnoreCase) }; ` +
    `if ($hit) { [void]$ids.Add($id); $changed=$true } }; if (-not $changed) { break } }; ` +
    `$ids | Where-Object { $_ -ne $me -and $_ -ne 0 } | ForEach-Object { Stop-Process -Id $_ -Force -ErrorAction SilentlyContinue }`;
  spawnSync("powershell", ["-NoProfile", "-Command", script], { windowsHide: true, timeout: 30000 });
}

function runOne({ cli, label, dir, model, timeoutMs, outDir, childEnv, runIndex, task, prompt, thinking, noSkills, tools = null, promptViaStdin = false }) {
  const started = Date.now();
  const isCmdShim = process.platform === "win32" && /\.(cmd|bat)$/i.test(cli);
  const program = isCmdShim ? process.env.ComSpec || "cmd.exe" : cli;
  // The thinking level is pinned identically for both CLIs: the user's real
  // settings say `defaultThinkingLevel: "max"`, and at max effort the model
  // spends its whole output budget on reasoning (measured: 15 368 thinking
  // deltas, zero tool calls, empty final message on this task).
  //
  // `noSkills` is the fairness control for discovery-path asymmetry: pir finds
  // skills under `<agentDir>/skills` (isolated by this harness), upstream pi
  // also reads `~/.agents/skills` (NOT isolatable by the agent dir), so without
  // this flag one leg runs with a 10.8 KB `<skills>` section and the other
  // without. Both CLIs accept `--no-skills`.
  const skillArgs = noSkills ? ["--no-skills"] : [];
  // `--no-session` keeps the persistent per-leg agent dirs free of per-run
  // transcripts and removes session-write time from startup. Both CLIs accept it.
  // The app prompt is sent on stdin: cmd.exe cannot safely carry a long
  // prompt containing quotes/JSON through an npm .cmd shim.
  const cliArgs = ["--mode", "json", "--print", "--model", model, "--thinking", thinking, "--no-session", ...skillArgs,
    ...(tools ? ["--tools", tools] : [])];
  if (!promptViaStdin) cliArgs.push(prompt);
  const spawnArgs = isCmdShim ? ["/d", "/c", cli, ...cliArgs] : cliArgs;
  // A spawn failure must still yield a run result: `main` dereferences
  // metrics/verdict/artifacts for the report. `spawn` can THROW synchronously
  // on Windows (e.g. an existing file that is not a valid executable), in
  // addition to emitting an async 'error'.
  const spawnFailureResult = (spawnError) => ({
    spawnError,
    metrics: {
      label, run: runIndex, wallMs: Date.now() - started,
      startupMs: null, requestAtMs: null, ttftMs: null, ttftFromRequestMs: null,
      exitCode: null, timedOut: false,
      turns: 0, toolCalls: [], toolErrors: 0,
      usage: { input: 0, output: 0, totalTokens: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0 },
      peakRssMb: 0, unparsedLines: 0, deltas: 0, stopReason: null, spawnError,
    },
    artifacts: { files: [], rows: [], bytes: 0, lines: 0 },
    verdict: { ok: false, notes: [`spawn failed: ${spawnError}`] },
  });
  let child;
  try {
    child = spawn(program, spawnArgs, {
      cwd: dir,
      windowsHide: true,
      env: childEnv,
      stdio: [promptViaStdin ? "pipe" : "ignore", "pipe", "pipe"],
    });
    if (promptViaStdin && child.stdin) {
      child.stdin.on("error", () => { /* the run verdict reports a failed leg */ });
      child.stdin.end(prompt);
    }
  } catch (error) {
    return Promise.resolve(spawnFailureResult(String(error?.message ?? error)));
  }

  const rss = { peakMb: 0, timer: null };
  if (child.pid !== undefined) samplePeakTreeRssMb(child, rss);

  const events = [];
  let buffer = "";
  let firstLineAt = null;
  let requestAt = null;
  let ttftAt = null;
  let killed = false;
  const stderrChunks = [];
  let forceSettleRef = null;
  let forceTimer = null;

  // Each `message_update` frame carries the FULL cumulative assistant message,
  // so retaining every parsed frame is O(n²) in message text for a long
  // generation. Keep only what the metrics need (type, arrival time, turn and
  // the assistant-event kind) and fold the metrics' work
  // into the parse loop; `events-*.jsonl` then records the stream's shape
  // without the quadratic payload.
  const compactEvent = (parsed) => {
    if (parsed.type !== "message_update") return parsed;
    return {
      type: parsed.type,
      _at: parsed._at,
      turn: parsed.turn ?? null,
      assistantEvent: parsed.assistantMessageEvent?.type ?? null,
    };
  };

  const timer = setTimeout(() => {
    killed = true;
    killRunTree(child, dir);
    // The tree kill is best-effort (taskkill/PowerShell can fail or miss an
    // MSYS-descended grandchild holding the pipes). Never let a timed-out leg
    // hang the campaign: force a 124 result after a bounded grace period.
    forceTimer = setTimeout(() => {
      try { child.stdout?.destroy(); } catch {}
      try { child.stderr?.destroy(); } catch {}
      if (forceSettleRef) forceSettleRef();
    }, 5_000);
  }, timeoutMs);

  // `close` waits for BOTH exit and closed stdio pipes. A survivor that
  // inherited the pipes (pir -> MSYS bash -> node) can hold them open forever,
  // so a timed-out leg never finalizes — measured: a 900 s timeout left the
  // whole app campaign wedged 1h42m with the agent long dead. Once the agent
  // process itself exits, give the pipes 5 s and then close OUR ends, which
  // lets `close` fire and the leg finalize.
  child.on("exit", () => {
    setTimeout(() => {
      try { child.stdout?.destroy(); } catch {}
      try { child.stderr?.destroy(); } catch {}
    }, 5000);
  });

  child.stdout?.on("data", (chunk) => {
    buffer += chunk.toString("utf8");
    let index;
    while ((index = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, index).trim();
      buffer = buffer.slice(index + 1);
      if (!line) continue;
      if (firstLineAt === null) firstLineAt = Date.now();
      let parsed;
      try {
        parsed = JSON.parse(line);
      } catch {
        parsed = { type: "_unparsed", line: line.slice(0, 300) };
      }
      // `assistantMessageEvent.delta` is the per-delta text of text_delta,
      // thinking_delta and toolcall_delta — exactly the frames the old
      // `JSON.stringify(parsed).includes('"delta"')` test matched, without
      // re-serializing the cumulative message on every line.
      const streamEvent = parsed.assistantMessageEvent;
      // `message_update/start` is emitted when the request leaves the CLI;
      // `ttftFromRequestMs` below is the provider-side first-token latency,
      // with process/Node boot removed.
      if (requestAt === null && parsed.type === "message_update"
          && streamEvent?.type === "start") {
        requestAt = Date.now();
      }
      if (ttftAt === null && parsed.type === "message_update"
          && streamEvent && typeof streamEvent.delta === "string") {
        ttftAt = Date.now();
      }
      parsed._at = Date.now() - started;
      events.push(compactEvent(parsed));
    }
  });
  child.stderr?.on("data", (chunk) => stderrChunks.push(chunk.toString("utf8")));

  return new Promise((resolveDone) => {
    let settled = false;
    const settle = (payload) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (forceTimer !== null) clearTimeout(forceTimer);
      if (rss.timer) clearInterval(rss.timer);
      resolveDone(payload);
    };
    const baseMetrics = (extra = {}) => ({
      label, run: runIndex, wallMs: Date.now() - started,
      startupMs: firstLineAt === null ? null : firstLineAt - started,
      requestAtMs: requestAt === null ? null : requestAt - started,
      ttftMs: ttftAt === null ? null : ttftAt - started,
      ttftFromRequestMs: ttftAt === null || requestAt === null || ttftAt < requestAt
        ? null
        : ttftAt - requestAt,
      exitCode: null,
      timedOut: killed,
      turns: 0,
      toolCalls: [],
      toolErrors: 0,
      usage: { input: 0, output: 0, totalTokens: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0 },
      peakRssMb: Math.round(rss.peakMb * 10) / 10,
      unparsedLines: 0,
      deltas: 0,
      stopReason: null,
      ...extra,
    });
    const emptyArtifacts = () => ({ files: [], rows: [], bytes: 0, lines: 0 });
    const safeArtifacts = () => {
      try {
        return collectArtifacts(dir);
      } catch {
        return emptyArtifacts();
      }
    };
    // Called by the timeout grace timer when the process tree survived the
    // kill. Produces a normal FAIL result instead of leaving the promise
    // pending forever.
    forceSettleRef = () => {
      settle({
        metrics: baseMetrics({
          wallMs: Date.now() - started,
          exitCode: 124,
          timedOut: true,
        }),
        artifacts: safeArtifacts(),
        verdict: {
          ok: false,
          notes: ["timed out; process tree survived the kill grace (forced 124)"],
        },
      });
    };

    child.on("error", (error) => {
      // A spawn failure has no metrics/verdict/artifacts; `main` dereferences
      // all three, so synthesize a FAIL run instead of crashing the campaign.
      const spawnError = String(error?.message ?? error);
      try {
        if (child.pid !== undefined) killRunTree(child, dir);
      } catch {}
      settle({
        spawnError,
        metrics: baseMetrics({ spawnError }),
        artifacts: safeArtifacts(),
        verdict: { ok: false, notes: [`spawn failed: ${spawnError}`] },
      });
    });
    child.on("close", async (code) => {
      // The async handler must ALWAYS settle: a throw in artifact collection or
      // the verdict (stat racing a deleted file, readFileSync on a locked
      // file, …) would otherwise leave the promise pending forever.
      let metrics;
      let artifacts;
      let verdict;
      try {
        metrics = baseMetrics({ wallMs: Date.now() - started, exitCode: code ?? null });
        const openTurns = new Map();
        for (const event of events) {
          switch (event.type) {
            case "turn_start": openTurns.set(event.turn, event._at); break;
            case "turn_end": openTurns.delete(event.turn); break;
            case "tool_execution_end":
              metrics.toolCalls.push({
                toolName: event.toolName, durationMs: event.durationMs ?? null,
                isError: Boolean(event.isError), atMs: event._at,
              });
              if (event.isError) metrics.toolErrors += 1;
              break;
            case "message_end": {
              const message = event.message ?? {};
              if (message.role === "assistant") {
                const u = message.usage ?? {};
                metrics.usage.input += u.input ?? 0;
                metrics.usage.output += u.output ?? 0;
                metrics.usage.totalTokens += u.totalTokens ?? 0;
                metrics.usage.cacheRead += u.cacheRead ?? 0;
                metrics.usage.cacheWrite += u.cacheWrite ?? 0;
                metrics.usage.reasoning += u.reasoning ?? 0;
                if (message.stopReason) metrics.stopReason = message.stopReason;
                // Final assistant text of the last message (probe verdict).
                const text = (Array.isArray(message.content) ? message.content : [])
                  .filter((part) => part?.type === "text")
                  .map((part) => part.text ?? "")
                  .join("");
                if (text) metrics.finalText = text.slice(0, 400);
              }
              break;
            }
            case "message_update":
              if (/_delta$/.test(event.assistantEvent ?? "")) metrics.deltas += 1;
              break;
            case "_unparsed": metrics.unparsedLines += 1; break;
            default: break;
          }
        }
        // `turns` counts `turn_start` EVENTS, not distinct `turn` values: upstream
        // `pi` emits `turn_start` without a `turn` field (`None`), so a
        // set-based count collapsed its 102/22 turns into 1 (measured).
        metrics.turns = events.filter((e) => e.type === "turn_start").length;

        artifacts = collectArtifacts(dir);
        if (metrics.timedOut) {
          // Do not spend the verdict budget on a half-written tree.
          verdict = { ok: false, notes: ["timed out"] };
        } else {
          verdict = task === "app" ? await nodeAppVerdict(dir) : artifactVerdict(dir);
        }
      } catch (error) {
        const failure = String(error?.stack ?? error?.message ?? error);
        metrics ??= baseMetrics({ wallMs: Date.now() - started, exitCode: code ?? null });
        metrics.harnessError = failure;
        artifacts ??= emptyArtifacts();
        verdict = { ok: false, notes: [`harness failed to finalize the run: ${failure}`] };
      }
      try {
        writeFileSync(join(outDir, `events-${label}-r${runIndex}.jsonl`), events.map((e) => JSON.stringify(e)).join("\n"));
        writeFileSync(join(outDir, `stderr-${label}-r${runIndex}.log`), stderrChunks.join(""));
      } catch (error) {
        metrics.harnessError = `failed to write run logs: ${error?.message ?? error}`;
      }
      settle({ metrics, artifacts, verdict });
    });
  });
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

function fmt(value, suffix = "") {
  return value === null || value === undefined ? "—" : `${value}${suffix}`;
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
  const version = result.status === 0
    ? stdout.split(/\r?\n/).map((line) => line.trim()).filter(Boolean)[0] ?? null
    : null;
  return {
    status: result.status ?? null,
    version,
    error: result.error ? String(result.error.message ?? result.error) : null,
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
  try {
    info.realpath = realpathSync(info.path);
  } catch {
    info.realpath = info.path;
  }
  try {
    const stat = statSync(info.realpath);
    info.mtime = stat.mtime.toISOString();
    info.mtimeMs = stat.mtimeMs;
    info.bytes = stat.size;
  } catch {}
  try {
    info.sha256 = hashFileSha256(info.realpath);
  } catch (error) {
    info.sha256 = null;
    info.hashError = String(error?.message ?? error);
  }
  const version = runBinaryVersion(info.realpath);
  info.version = version.version;
  info.versionExitCode = version.status;
  if (version.error) info.versionError = version.error;
  return info;
}

function samePath(a, b) {
  return process.platform === "win32"
    ? a.toLowerCase() === b.toLowerCase()
    : a === b;
}

function assertDistinctBinaries(pirInfo, piInfo) {
  if (!pirInfo.exists || !piInfo.exists) return;
  if (pirInfo.sha256 && piInfo.sha256 && pirInfo.sha256 === piInfo.sha256) {
    throw new Error(
      `the two legs resolve to identical binaries (sha256 ${pirInfo.sha256}); ` +
      `pass a real upstream pi via --pi-cli or fix the launchers`,
    );
  }
  if (pirInfo.realpath && piInfo.realpath && samePath(pirInfo.realpath, piInfo.realpath)) {
    throw new Error(`the two legs resolve to the same path: ${pirInfo.realpath}`);
  }
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
        if (entry === "work" || entry === "report.md" || entry === "report.json" || entry === "summary.json"
            || /^events-/.test(entry) || /^stderr-/.test(entry)) {
          rmSync(join(resolved, entry), { recursive: true, force: true });
        }
      }
    }
  } else {
    mkdirSync(resolved, { recursive: true });
  }
}

/**
 * Isolated agent dir for ONE leg. Every run gets its own directory: a shared
 * dir would let a session/cache/config write from the first leg leak into the
 * second (the standalone-invocation leak this harness previously had).
 */
function createAgentDir(realAgentDir) {
  const dir = mkdtempSync(join(tmpdir(), "pir-ab-agent-"));
  try {
    for (const name of ["models.json", "settings.json", "auth.json"]) {
      const src = join(realAgentDir, name);
      if (existsSync(src)) writeFileSync(join(dir, name), readFileSync(src));
    }
    return dir;
  } catch (error) {
    rmSync(dir, { recursive: true, force: true });
    throw error;
  }
}

/**
 * Apply the fairness knobs to a per-leg agent dir and return the snapshot that
 * goes into the report, so both legs are provably configured identically:
 *  - `defaultThinkingLevel: <thinking>` (the user's file says "max", which
 *    burns the whole output budget on reasoning — measured: 15 368 thinking
 *    deltas, 0 tool calls, empty final message);
 *  - drop `packages`: no user extension set loads in either CLI, so the
 *    comparison isolates the agent CLIs themselves (extension-runtime costs are
 *    measured separately by scripts/perf/ext_delta_throughput.py).
 */
function prepareAgentDir(realAgentDir, thinking) {
  const agentDir = createAgentDir(realAgentDir);
  const settingsPath = join(agentDir, "settings.json");
  const snapshot = { defaultThinkingLevel: null, packages: null, raw: null };
  try {
    if (existsSync(settingsPath)) {
      try {
        const settings = JSON.parse(readFileSync(settingsPath, "utf8"));
        snapshot.defaultThinkingLevel = settings.defaultThinkingLevel ?? null;
        snapshot.packages = settings.packages ?? null;
        settings.defaultThinkingLevel = thinking;
        delete settings.packages;
        snapshot.raw = settings;
        writeFileSync(settingsPath, JSON.stringify(settings, null, 2));
      } catch (error) {
        process.stdout.write(`[ab-bird] settings patch skipped: ${error?.message ?? error}\n`);
      }
    }
  } catch (error) {
    rmSync(agentDir, { recursive: true, force: true });
    throw error;
  }
  return { dir: agentDir, snapshot };
}

function archiveWorkDir(source, destination) {
  rmSync(destination, { recursive: true, force: true });
  mkdirSync(dirname(destination), { recursive: true });
  cpSync(source, destination, { recursive: true });
}

/** Median of the finite values (null when none). Medians, never means. */
function median(values) {
  const nums = values.filter((v) => Number.isFinite(v)).sort((a, b) => a - b);
  if (nums.length === 0) return null;
  const mid = Math.floor(nums.length / 2);
  return nums.length % 2 === 1 ? nums[mid] : Math.round((nums[mid - 1] + nums[mid]) / 2);
}

/**
 * Per-leg summary across repetitions. Only runs that are not dirty (no
 * timeout, a TTFT within the stall rule) enter the medians; a stalled run is
 * listed in `dropped` instead of being averaged in.
 */
function summarizeRuns(runs, { wallLimitMs, ttftLimitMs }) {
  const byLeg = new Map();
  for (const run of runs) {
    const m = run.metrics ?? {};
    const label = m.label ?? "?";
    if (!byLeg.has(label)) byLeg.set(label, []);
    byLeg.get(label).push(run);
  }
  const summary = [];
  for (const [label, legRuns] of byLeg) {
    const kept = [];
    const dropped = [];
    for (const run of legRuns) {
      const m = run.metrics ?? {};
      const ttft = Number.isFinite(m.ttftFromRequestMs) ? m.ttftFromRequestMs : m.ttftMs;
      const reasons = [];
      if (m.timedOut) reasons.push("timedOut");
      if (m.spawnError) reasons.push("spawnError");
      if (!Number.isFinite(ttft)) reasons.push("ttft-missing");
      else if (ttft > ttftLimitMs) reasons.push(`ttft>${ttftLimitMs}ms`);
      // The wall rule applies to the short probe tasks only; long artifact
      // tasks legitimately take minutes.
      if (run.task === "probe" || run.task === "gen") {
        if (!Number.isFinite(m.wallMs)) reasons.push("wall-missing");
        else if (m.wallMs > wallLimitMs) reasons.push(`wall>${wallLimitMs}ms`);
      }
      const entry = { run: m.run ?? null, verdict: run.verdict?.ok === true, reasons };
      if (reasons.length > 0) dropped.push(entry);
      else kept.push({ run, entry });
    }
    const pick = (fn) => median(kept.map((item) => fn(item.run)));
    summary.push({
      label,
      runs: legRuns.length,
      kept: kept.map((item) => item.entry.run),
      dropped,
      verdicts: { pass: legRuns.filter((r) => r.verdict?.ok === true).length, fail: legRuns.filter((r) => r.verdict?.ok !== true).length },
      median: {
        wallMs: pick((r) => r.metrics?.wallMs),
        startupMs: pick((r) => r.metrics?.startupMs),
        ttftMs: pick((r) => r.metrics?.ttftMs),
        ttftFromRequestMs: pick((r) => r.metrics?.ttftFromRequestMs),
        peakRssMb: pick((r) => r.metrics?.peakRssMb),
        totalTokens: pick((r) => r.metrics?.usage?.totalTokens),
      },
    });
  }
  return { rule: { wallMs: wallLimitMs, ttftMs: ttftLimitMs }, legs: summary };
}

function checkKind(task) {
  if (task === "bird") return { kind: "artifact-shape", quality: false, note: "产物形状检查，不是隐藏测试质量分" };
  if (task === "app") return { kind: "artifact-self-test", quality: false, note: "项目自带测试（自评），不能证明实现质量" };
  return { kind: "probe", quality: null, note: "请求延迟探针" };
}

/**
 * Stale-binary gate: a release `pir` older than src/Cargo.toml/Cargo.lock
 * produces a meaningless A/B. Only paths with a `release` segment are gated.
 */
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

function checkReleaseFreshness(pirInfo) {
  if (!pirInfo.exists || !pirInfo.realpath || !/(^|[\\/])release([\\/]|$)/i.test(pirInfo.realpath)) {
    return { checked: false, reason: "not a release path" };
  }
  const newestSource = Math.max(
    newestMtimeMs(join(REPO, "src")),
    newestMtimeMs(join(REPO, "Cargo.toml")),
    newestMtimeMs(join(REPO, "Cargo.lock")),
  );
  const binaryMtime = pirInfo.mtimeMs ?? 0;
  const stale = newestSource > binaryMtime + 1_000;
  return {
    checked: true,
    stale,
    binaryMtime: pirInfo.mtime ?? null,
    newestSourceMtime: newestSource > 0 ? new Date(newestSource).toISOString() : null,
  };
}

/**
 * Anti-fraud guard for the run_ab path: when the launcher's *identity* target
 * is supplied out-of-band, the launcher MUST actually launch that binary.
 * Otherwise a launcher could point at a different (e.g. patched) build while
 * the report claims the hashed one.
 */
function assertLauncherIdentity(label, launcherPath, identityPath) {
  const launched = resolveCmdLauncher(resolve(launcherPath));
  if (!launched) return { checked: false, reason: "not a parseable .cmd/.bat launcher" };
  let launchedReal;
  try { launchedReal = realpathSync(launched); } catch { launchedReal = launched; }
  let identityReal;
  try { identityReal = realpathSync(resolve(identityPath)); } catch { identityReal = resolve(identityPath); }
  if (!samePath(launchedReal, identityReal)) {
    throw new Error(
      `${label} launcher ${launcherPath} executes ${launchedReal}, but the recorded identity binary is ${identityReal}; ` +
      `refusing a run whose hash would describe a different binary`,
    );
  }
  return { checked: true, launcher: resolve(launcherPath), launches: launchedReal };
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    process.stdout.write(`${USAGE}\n`);
    return 0;
  }
  // `--judge <dir>`: run the SAME verdict function on an artifact set that
  // already exists, so a set can be re-scored without re-running a CLI (used to
  // establish the true verdict of the first app run, whose harness-side parse
  // and node --test handling were both broken).
  if (args.judge) {
    const dir = resolve(args.judge);
    if (!existsSync(dir)) throw new Error(`--judge dir not found: ${dir}`);
    const verdict = args.task === "app" ? await nodeAppVerdict(dir) : artifactVerdict(dir);
    const artifacts = collectArtifacts(dir);
    process.stdout.write(`${JSON.stringify({ dir, task: args.task, files: artifacts.files, bytes: artifacts.bytes, lines: artifacts.lines, verdict }, null, 2)}\n`);
    return;
  }
  const outDir = resolve(args.out);
  // Dry run: validate arguments and print the plan only. No binary is resolved
  // or hashed, no --out dir is created, no agent dir is copied and no key is
  // read — so it works on a machine with neither CLI installed and no gateway.
  if (args.dryRun) {
    const planLegs = args.legs === "both" ? ["pir", "pi"] : [args.legs];
    const plan = {
      script: fileURLToPath(import.meta.url),
      task: args.task,
      checkKind: checkKind(args.task),
      runs: args.runs,
      legs: planLegs,
      interleaved: planLegs.length > 1,
      order: Array.from({ length: args.runs }, () => planLegs).flat(),
      pir: args.pir,
      pi: args.piCli,
      model: args.model,
      thinking: args.thinking,
      timeoutMs: args.timeoutMs,
      out: outDir,
      noSkills: args.noSkills,
      promptFile: args.promptFile,
      agentDirSource: args.agentDir ?? "(real ~/.pi/agent)",
      agentDir: "per-leg temp dir (created fresh for every run, removed afterwards)",
    };
    process.stdout.write("[ab-bird] dry run — nothing is started, no binary is resolved\n");
    for (const [key, value] of Object.entries(plan)) {
      if (key === "script" || key === "checkKind") continue;
      process.stdout.write(`[ab-bird] ${key.padEnd(12)} ${Array.isArray(value) ? JSON.stringify(value) : value}\n`);
    }
    const qualityLabel = plan.checkKind.quality === true ? "yes" : plan.checkKind.quality === false ? "no" : "n/a (latency probe)";
    process.stdout.write(`[ab-bird] checkKind    ${plan.checkKind.kind} (quality score: ${qualityLabel} — ${plan.checkKind.note})\n`);
    const qualityHint = "node scripts/perf/harder_tasks_campaign.mjs  (hidden-test quality scores)";
    if (plan.checkKind.quality !== true) process.stdout.write(`[ab-bird] quality      ${qualityHint}\n`);
    return 0;
  }
  const wantedLegs = args.legs === "both" ? ["pir", "pi"] : [args.legs];
  args.pir = resolve(args.pir);
  if (wantedLegs.includes("pir") && !existsSync(args.pir)) throw new Error(`pir not found: ${args.pir}`);
  if (wantedLegs.includes("pi") && !existsSync(args.piCli)) throw new Error(`pi CLI not found: ${args.piCli} (pass --pi-cli)`);

  // Identity is measured on the real targets when run_ab supplies them; the
  // arguments may be launchers (.cmd) that only pin an agent dir. The launcher
  // is then REQUIRED to execute exactly the recorded identity binary.
  const identityPirPath = process.env.PIR_AB_PIR_BIN || args.pir;
  const identityPiPath = process.env.PIR_AB_PI_BIN || args.piCli;
  const identityChecks = {
    pir: wantedLegs.includes("pir") && process.env.PIR_AB_PIR_BIN
      ? assertLauncherIdentity("pir", args.pir, identityPirPath) : { checked: false },
    pi: wantedLegs.includes("pi") && process.env.PIR_AB_PI_BIN
      ? assertLauncherIdentity("pi", args.piCli, identityPiPath) : { checked: false },
  };
  const binaries = {
    pir: wantedLegs.includes("pir")
      ? { launcher: args.pir, ...binaryInfo("pir", identityPirPath) }
      : { skipped: "leg not selected" },
    pi: wantedLegs.includes("pi")
      ? { launcher: args.piCli, ...binaryInfo("pi", identityPiPath) }
      : { skipped: "leg not selected" },
  };
  if (wantedLegs.length === 2) assertDistinctBinaries(binaries.pir, binaries.pi);
  const freshness = binaries.pir.exists ? checkReleaseFreshness(binaries.pir) : { checked: false, reason: "pir leg not selected" };
  if (freshness.stale && !args.allowStale) {
    // A stale release binary is only rejected by run_ab today; standalone runs
    // could silently compare an old build.
    throw new Error(
      `pir release binary is older than src/Cargo.toml/Cargo.lock: ${binaries.pir.realpath} ` +
      `(${freshness.binaryMtime} < ${freshness.newestSourceMtime}); rebuild or pass --allow-stale`,
    );
  }
  // Validate the agent-config source before creating anything.
  const realAgentDir = args.agentDir
    ? resolve(args.agentDir)
    : join(process.env.USERPROFILE ?? process.env.HOME ?? "", ".pi", "agent");
  if (args.agentDir && !existsSync(realAgentDir)) {
    throw new Error(`--agent-dir not found: ${realAgentDir}`);
  }
  // Only now — after every validation — touch the artifacts dir, so a refused
  // invocation leaves no empty directory behind.
  prepareOutDir(outDir, args.force);

  const task = TASKS[args.task];
  let prompt = "";
  let settingsSnapshot = null;

  const runs = [];
  try {
    prompt = args.promptFile ? readFileSync(resolve(args.promptFile), "utf8") : task.prompt;
    for (let repetition = 1; repetition <= args.runs; repetition += 1) {
      const legs = [
        { cli: args.pir, label: "pir" },
        { cli: args.piCli, label: "pi" },
      ].filter((leg) => wantedLegs.includes(leg.label));
      for (const target of legs) {
        // Per-LEG agent dir: one temp dir per run, never one shared between the
        // legs (a shared dir leaks the first leg's settings/session/cache into
        // the second).
        const prepared = prepareAgentDir(realAgentDir, args.thinking);
        settingsSnapshot ??= prepared.snapshot;
        const childEnv = { ...process.env, PIR_AGENT_DIR: prepared.dir, PI_CODING_AGENT_DIR: prepared.dir };
        // Keep the agent's cwd outside the repo. The finished tree is archived
        // into outDir/work for retention; the temp dir is removed afterwards.
        const workDir = mkdtempSync(join(tmpdir(), `pir-ab-${args.task}-${target.label}-r${repetition}-`));
        const keepDir = join(outDir, "work", `${args.task}-${target.label}-r${repetition}`);
        process.stdout.write(`[ab-bird] ${target.label} run ${repetition} …\n`);
        let result;
        try {
          result = await runOne({
            cli: target.cli, label: target.label, dir: workDir, model: args.model,
            timeoutMs: args.timeoutMs, outDir, childEnv, runIndex: repetition,
            task: args.task, prompt, thinking: args.thinking, noSkills: args.noSkills,
            tools: args.tools,
            promptViaStdin: args.task === "app",
          });
          archiveWorkDir(workDir, keepDir);
        } finally {
          rmSync(workDir, { recursive: true, force: true });
          rmSync(prepared.dir, { recursive: true, force: true });
        }
        // Probe verdict: the run must terminate by itself with an assistant
        // reply (no tool use), so wall clock = startup + one round trip. A
        // spawn error already has a synthesized FAIL verdict; keep its note.
        if (task.verdict === "probe" && !result.spawnError) {
          const m0 = result.metrics ?? {};
          const failures = [];
          const warnings = [];
          if (m0.timedOut) failures.push("timed out");
          if (m0.exitCode !== 0) failures.push(`exitCode=${m0.exitCode}`);
          if ((m0.toolCalls ?? []).length) failures.push(`used ${m0.toolCalls.length} tools`);
          if (m0.turns !== 1) failures.push(`turns=${m0.turns}`);
          if (m0.stopReason === "error" || m0.stopReason === "aborted") {
            failures.push(`stopReason=${m0.stopReason}`);
          } else if (!m0.stopReason) {
            warnings.push("stopReason missing");
          }
          if (m0.ttftMs === null || m0.ttftMs === undefined) {
            failures.push("ttft missing (no delta observed)");
          }
          if (m0.ttftFromRequestMs === null || m0.ttftFromRequestMs === undefined) {
            warnings.push("request-relative TTFT missing (no message_update/start)");
          }
          const finalText = typeof m0.finalText === "string" ? m0.finalText.trim() : "";
          if (!finalText) failures.push("empty final text");
          else if (finalText !== "OK") warnings.push(`final text not exactly "OK": ${JSON.stringify(finalText).slice(0, 80)}`);
          result.verdict = { ok: failures.length === 0, notes: [...failures, ...warnings] };
        }
        runs.push({ task: args.task, prompt, dir: keepDir, ...result });
        const m = result.metrics ?? {};
        process.stdout.write(
          `[ab-bird] ${target.label} r${repetition}: verdict=${result.verdict.ok ? "PASS" : "FAIL"} ` +
          `wall=${fmt(m.wallMs, "ms")} ttft=${fmt(m.ttftMs, "ms")} ttft_req=${fmt(m.ttftFromRequestMs, "ms")} ` +
          `turns=${fmt(m.turns)} tools=${fmt(m.toolCalls?.length)} tokens=${fmt(m.usage?.totalTokens)} ` +
          `rss=${fmt(m.peakRssMb, "MB")} files=${result.artifacts.files.length} bytes=${result.artifacts.bytes} ` +
          `reply=${JSON.stringify(m.finalText ?? "").slice(0, 40)}\n`,
        );
      }
    }
  } finally {
    // Per-leg agent dirs are removed by the inner finally; nothing shared
    // survives here.
  }

  settingsSnapshot ??= { defaultThinkingLevel: null, packages: null, raw: null };
  const kind = checkKind(args.task);
  const summary = summarizeRuns(runs, { wallLimitMs: args.stallWallMs, ttftLimitMs: args.stallTtftMs });
  const title = args.task === "bird"
    ? "愤怒的小鸟 Web 版（ARTIFACT CHECK）"
    : args.task === "app"
      ? "书签管理 API（SELF-EVAL CHECK）"
      : "短探针（纯请求/响应开销）";
  const lines = [];
  lines.push(`# A/B: ${title}（pi vs pir）`);
  lines.push("");
  if (kind.quality === false) {
    lines.push(`> **这不是质量分。** 本任务类型是 \`${kind.kind}\`：${kind.note}。`);
    lines.push("> 隐藏测试质量分请用 `scripts/perf/harder_tasks_campaign.mjs` 或 `scripts/perf/p3_task_e2e.mjs`（`ab-ar`: `--task quality`）。");
    lines.push("");
  }
  lines.push(`- 时间：${new Date().toISOString()}`);
  lines.push(`- task：\`${args.task}\`（checkKind=\`${kind.kind}\`, quality=${kind.quality === null ? "n/a" : kind.quality}）；提示词（两条腿逐字相同）：${prompt}`);
  lines.push(`- 模型/网关：\`${args.model}\`（同一网关；串行交替执行，避免并发上限影响）`);
  lines.push(`- thinking：两条腿都钉 \`--thinking ${args.thinking}\`，临时 settings 的 \`defaultThinkingLevel\` 同步改 ${args.thinking}${settingsSnapshot.defaultThinkingLevel ? `（原值 ${settingsSnapshot.defaultThinkingLevel}）` : ""}`);
  lines.push(`- settings 快照（隔离 agent dir，两腿逐字相同）：\`${JSON.stringify(settingsSnapshot.raw)}\``);
  lines.push(`- 扩展：临时 settings 移除 \`packages\`${settingsSnapshot.packages ? `（原 packages: ${JSON.stringify(settingsSnapshot.packages)}）` : ""}`);
  lines.push(`- session：两条腿都传 \`--no-session\`（不写入会话记录，启动口径一致）`);
  lines.push(
    args.noSkills
      ? "- skills：两条腿都传 `--no-skills`（公平控制：pir 的 skills 在 `<agentDir>/skills`，pi 还读 `~/.agents/skills`，隔离 agent dir 只挡掉 pir 的）"
      : "- skills：未传 `--no-skills` —— pir 因 agent dir 隔离看不到 skills，pi 仍会读 `~/.agents/skills`（前缀不对称，见 script 注释）",
  );
  lines.push(`- 隔离：工作目录在系统临时目录（运行后归档到 \`${outDir.replace(/\\/g, "/")}/work/\`）；**每条腿每次运行**都新建独立 agent dir（用后删除，两条腿不共享）`);
  lines.push(`- binaries：pir \`${binaries.pir.realpath ?? binaries.pir.path}\` sha256=${binaries.pir.sha256 ?? "—"} version=${JSON.stringify(binaries.pir.version ?? "—")}；pi \`${binaries.pi.realpath ?? binaries.pi.path}\` sha256=${binaries.pi.sha256 ?? "—"} version=${JSON.stringify(binaries.pi.version ?? "—")}`);
  lines.push(`- 陈旧二进制检查：${freshness.checked ? (freshness.stale ? `**STALE**（binary ${freshness.binaryMtime} < src ${freshness.newestSourceMtime}）` : `ok（binary ${freshness.binaryMtime} ≥ src ${freshness.newestSourceMtime}）`) : freshness.reason}`);
  if (identityChecks.pir.checked || identityChecks.pi.checked) {
    lines.push(`- launcher 一致性：pir=${JSON.stringify(identityChecks.pir)}；pi=${JSON.stringify(identityChecks.pi)}`);
  }
  lines.push(
    task.verdict === "artifacts"
      ? "- 判定（两条腿同一函数；产物形状检查，不验证可玩性）：index.html 存在且含 <canvas> 与 <script>、引用的本地脚本存在；所有 .js 通过 `node --check`；整个产物集 ≥8 KB 且有 README.md"
      : task.verdict === "node-app"
        ? "- 判定（两条腿同一函数；自评测试，不能证明实现质量）：所有 .js 过 `node --check`；`node --test` 退出码 0 且通过 ≥4 条测试；产物集 ≥8 KB；有 README.md 与静态 UI HTML"
        : "- 判定（probe）：未超时、退出码 0、恰好 1 轮、0 工具调用、stopReason 非 error/aborted、TTFT 非空、最终文本非空（理想为 `OK`）；墙钟 = 启动 + 一次往返",
  );
  lines.push(`- RSS：**进程树工作集合计**（PowerShell CIM 每 3 s 采样；pi 经 cmd.exe 垫片启动，单采 pid 只会量到垫片）；启动 = 第一行 JSON 输出；TTFT = 进程启动→首个 delta；TTFT(req) = request 发出（message_update/start）→首个 delta`);
  // Per-leg medians over the STALL-FREE runs only, with the dropped runs
  // listed beside them. Ranked on verdict first, then these medians.
  lines.push("## 每条腿的中位数（仅无停滞的运行；中位数，不用平均值）");
  lines.push("");
  lines.push(`- 丢弃规则：timedOut / spawnError / TTFT(req) > ${args.stallTtftMs} ms${args.task === "probe" || args.task === "gen" ? ` / wall > ${args.stallWallMs} ms` : ""}。`);
  lines.push("| CLI | 运行数 | 保留 | 丢弃 | 判定(PASS/FAIL) | 中位墙钟 | 中位启动 | 中位 TTFT | 中位 TTFT(req) | 中位峰值 RSS | 中位 tokens |");
  lines.push("|---|---|---|---|---|---|---|---|---|---|---|");
  for (const leg of summary.legs) {
    lines.push(
      `| ${leg.label} | ${leg.runs} | ${leg.kept.length ? leg.kept.join(",") : "—"} | ` +
      `${leg.dropped.length ? leg.dropped.map((d) => `r${d.run}:${d.reasons.join("+")}`).join("; ") : "—"} | ` +
      `${leg.verdicts.pass}/${leg.verdicts.fail} | ${fmt(leg.median.wallMs, "ms")} | ${fmt(leg.median.startupMs, "ms")} | ` +
      `${fmt(leg.median.ttftMs, "ms")} | ${fmt(leg.median.ttftFromRequestMs, "ms")} | ${fmt(leg.median.peakRssMb, " MB")} | ${fmt(leg.median.totalTokens)} |`,
    );
  }
  lines.push("");
  lines.push("");
  lines.push("## 每次运行明细");
  lines.push("");
  lines.push("| CLI | 轮次 | 判定 | 墙钟 | 启动 | TTFT | TTFT(req) | turns | tool calls | 工具错误 | deltas | tokens(in/out/total) | cacheRead | 峰值 RSS | 产物(files/bytes) | 产物行数 | stopReason | 退出码 |");
  lines.push("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
  for (const run of runs) {
    const m = run.metrics ?? {};
    const a = run.artifacts;
    const u = m.usage ?? {};
    lines.push(
      `| ${m.label} | ${m.run} | ${run.verdict.ok ? "PASS" : "FAIL"} | ${fmt(m.wallMs, "ms")} | ${fmt(m.startupMs, "ms")} | ` +
      `${fmt(m.ttftMs, "ms")} | ${fmt(m.ttftFromRequestMs, "ms")} | ${fmt(m.turns)} | ${fmt(m.toolCalls?.length)} | ${fmt(m.toolErrors)} | ${fmt(m.deltas)} | ` +
      `${fmt(u.input)}/${fmt(u.output)}/${fmt(u.totalTokens)} | ${fmt(u.cacheRead)} | ${fmt(m.peakRssMb, " MB")} | ` +
      `${a.files.length}/${a.bytes} | ${a.lines} | ${fmt(m.stopReason)} | ${fmt(m.exitCode)} |`,
    );
  }
  lines.push("");
  for (const run of runs) {
    const m = run.metrics ?? {};
    lines.push(`## ${m.label} · run ${m.run}`);
    lines.push("");
    lines.push(`- 判定：${run.verdict.ok ? "PASS" : "FAIL"}${run.verdict.notes.length ? ` — ${run.verdict.notes.join("; ")}` : ""}`);
    lines.push(`- 工具序列：${(m.toolCalls ?? []).map((c) => `${c.toolName}(${c.durationMs ?? "?"}ms${c.isError ? ",error" : ""})`).join(" → ") || "（无）"}`);
    if (m.finalText) lines.push(`- 回复：${JSON.stringify(m.finalText).slice(0, 300)}`);
    lines.push(`- 工作目录：\`${run.dir}\``);
    lines.push(`- 产物清单：`);
    for (const row of run.artifacts.rows) {
      lines.push(`  - \`${row.path}\` — ${row.bytes} B / ${row.lines} 行`);
    }
    if (m.timedOut) lines.push(`- **超时终止**（${args.timeoutMs} ms）`);
    if (m.unparsedLines) lines.push(`- 非 JSON 输出行：${m.unparsedLines}`);
    lines.push("");
  }
  writeFileSync(join(outDir, "report.md"), lines.join("\n"));
  // Machine-readable summary beside the human report: check kind (artifact vs
  // quality), binary identity, canary/stall rule, and per-leg stall-free
  // medians. `summary.json` is the file to diff between campaigns.
  const machineSummary = {
    createdAt: new Date().toISOString(),
    task: args.task,
    checkKind: kind,
    qualityScore: false,
    qualityHarnesses: [
      "scripts/perf/harder_tasks_campaign.mjs",
      "scripts/perf/p3_task_e2e.mjs",
      "scripts/perf/ab-ar/run_ab.mjs --task quality",
    ],
    model: args.model,
    endpoint: process.env.PIR_AB_BASE_URL ?? null,
    thinking: args.thinking,
    timeoutMs: args.timeoutMs,
    noSkills: args.noSkills,
    outDir: outDir.replace(/\\/g, "/"),
    binaries: {
      pir: { realpath: binaries.pir.realpath ?? null, sha256: binaries.pir.sha256 ?? null, version: binaries.pir.version ?? null, launcher: binaries.pir.launcher ?? null },
      pi: { realpath: binaries.pi.realpath ?? null, sha256: binaries.pi.sha256 ?? null, version: binaries.pi.version ?? null, launcher: binaries.pi.launcher ?? null },
      identicalRejected: true,
      freshness,
      launcherIdentity: identityChecks,
    },
    stallRule: summary.rule,
    legs: summary.legs,
    exitCode: null,
    runs: runs.map((run) => ({
      label: run.metrics?.label ?? null,
      run: run.metrics?.run ?? null,
      verdict: run.verdict?.ok === true,
      notes: run.verdict?.notes ?? [],
      metrics: {
        wallMs: run.metrics?.wallMs ?? null,
        startupMs: run.metrics?.startupMs ?? null,
        ttftMs: run.metrics?.ttftMs ?? null,
        ttftFromRequestMs: run.metrics?.ttftFromRequestMs ?? null,
        peakRssMb: run.metrics?.peakRssMb ?? null,
        turns: run.metrics?.turns ?? null,
        toolCalls: run.metrics?.toolCalls?.length ?? null,
        totalTokens: run.metrics?.usage?.totalTokens ?? null,
        timedOut: run.metrics?.timedOut ?? false,
      },
    })),
  };
  writeFileSync(join(outDir, "report.json"), JSON.stringify({
    task: args.task,
    checkKind: kind,
    prompt,
    model: args.model,
    thinking: args.thinking,
    settingsSnapshot,
    binaries,
    freshness,
    launcherIdentity: identityChecks,
    stallRule: summary.rule,
    legSummary: summary.legs,
    metricsDefinition: {
      wallMs: "process start -> close",
      startupMs: "process start -> first JSON stdout line",
      ttftMs: "process start -> first assistant delta",
      ttftFromRequestMs: "message_update/start -> first assistant delta",
      peakRssMb: "peak working set of the whole process tree, sampled every 3 s",
      summary: "medians over stall-free runs only (never means)",
    },
    runs,
  }, null, 2));
  // A spawn failure is a broken harness, not a model verdict.
  const spawnFailed = runs.some((run) => run.spawnError);
  const failed = runs.filter((r) => !r.verdict.ok).length;
  // 3 = model/artifact verdict FAIL; 2 = harness error; 0 = pass.
  const exitCode = spawnFailed ? 2 : (failed === 0 ? 0 : 3);
  machineSummary.exitCode = exitCode;
  writeFileSync(join(outDir, "summary.json"), `${JSON.stringify(machineSummary, null, 2)}\n`);
  process.stdout.write(`[ab-bird] report: ${join(outDir, "report.md")}\n`);
  process.stdout.write(`[ab-bird] summary: ${join(outDir, "summary.json")}\n`);
  return exitCode;
}

main().then((code) => process.exit(code)).catch((error) => {
  const message = String(error?.message ?? error);
  // Usage/config errors (unknown flag, --quality, bad value) are not crashes:
  // print the message, point at --help, and keep the exit code distinct from a
  // harness failure so scripts can tell them apart.
  if (/--help|needs a value|unknown flag|unknown task|must be|no quality score|not found|older than|identical binaries|same path|launcher .* executes|refusing/i.test(message)) {
    console.error(`[ab-bird] ${message}`);
    process.exit(1);
  }
  console.error(`[ab-bird] ${error?.stack ?? error}`);
  process.exit(2);
});
