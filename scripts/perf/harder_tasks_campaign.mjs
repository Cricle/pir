#!/usr/bin/env node
/**
 * Harder offline task campaign: 5 hidden-test tasks x {pir, pi}.
 *
 * Each leg gets a fresh temp work dir (outside the repo, so the agent cannot
 * walk up to the hidden tests), a fresh isolated agent dir, and the task
 * prompt verbatim. The hidden verdict runs after the leg and prints a JSON
 * score; the work dir is archived for inspection.
 *
 * Quality/intelligence signal: score = passed/total hidden checks.
 * Speed/cost signal: wall, startup, TTFT, turns, tool calls, tokens.
 *
 * Usage: node scripts/perf/harder_tasks_campaign.mjs [--runs 1] [--out dir]
 *        [--model openai/gpt-5] [--timeout-ms 900000] [--task name]
 *        [--pir <exe>] [--pi <cmd>] [--allow-stale] [--dry-run] [--help]
 *
 * `pir`/`pi` default to $PIR_BIN/$PI_CLI, then the repo build and the npm shim.
 * Binaries are recorded (realpath/SHA-256/version); identical binaries and a
 * release pir older than src/ are refused. Each leg gets a fresh agent dir and
 * a fresh work dir (outside the repo), both removed afterwards.
 */
import { execFile, spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const TASKS_DIR = join(HERE, "tasks");
const TASKS = ["retry-policy", "argparse-spec", "textstats-split", "logq", "date-range"];

const USAGE = [
  "Usage: node scripts/perf/harder_tasks_campaign.mjs [flags]",
  "",
  "  --runs N              repetitions (default 1)",
  "  --out <dir>           artifacts dir (outside the repo; empty unless --force)",
  "  --model <provider/id> model id (default openai/gpt-5)",
  "  --thinking <level>    pinned thinking level written to each agent dir (default low)",
  "  --timeout-ms N        per-leg timeout (default 900000)",
  "  --task <name|all>     one hidden-test task, or all (default all)",
  "  --pir <exe>           pir binary (default $PIR_BIN or target-alt/release/pir.exe)",
  "  --pi <cmd>            upstream pi entry point (default $PI_CLI or %APPDATA%/npm/pi.cmd)",
  "  --force               replace a non-empty --out dir",
  "  --allow-stale         accept a release pir older than src/ (frozen build)",
  "  --stall-ttft-ms N     TTFT over this marks a run non-stall-free (default 10000)",
  "  --dry-run             print the plan; resolves no binary, needs no key/network",
  "  -h, --help            this text",
  "",
  "Score = passed/total hidden checks. Timing medians are per leg, over",
  "stall-free runs only. Exit: 0 all checks pass · 3 any check fails · 2 harness error.",
].join("\n");

function parseArgs(argv) {
  const a = {
    runs: 1, out: null, model: "openai/gpt-5", timeoutMs: 900_000,
    task: null, force: false, pir: null, pi: null, allowStale: false,
    stallTtftMs: 10_000, dryRun: false, help: false, thinking: "low",
    // Equalized-surface control: when set, BOTH legs get the same
    // `--tools <list>`; without it each CLI uses its own default set (pi: 4
    // core tools; pir: core + default-active builtins), which makes a score
    // difference capability-driven instead of model-driven.
    tools: null,
  };
  for (let i = 0; i < argv.length; i++) {
    const n = () => argv[++i];
    switch (argv[i]) {
      case "--runs": a.runs = Number.parseInt(n(), 10); break;
      case "--out": a.out = n(); break;
      case "--model": a.model = n(); break;
      case "--timeout-ms": a.timeoutMs = Number.parseInt(n(), 10); break;
      case "--task": a.task = n(); break;
      case "--tools": a.tools = n(); break;
      case "--pir": a.pir = n(); break;
      case "--pi": a.pi = n(); break;
      case "--force": a.force = true; break;
      case "--allow-stale": a.allowStale = true; break;
      case "--stall-ttft-ms": a.stallTtftMs = Number.parseInt(n(), 10); break;
      case "--thinking": a.thinking = n(); break;
      case "--dry-run": a.dryRun = true; break;
      case "-h":
      case "--help": a.help = true; break;
      default: throw new Error(`unknown arg ${argv[i]} (see --help)`);
    }
  }
  if (!Number.isInteger(a.runs) || a.runs < 1) throw new Error(`--runs must be a positive integer, got ${a.runs}`);
  if (!Number.isInteger(a.timeoutMs) || a.timeoutMs < 1_000) throw new Error(`--timeout-ms must be >= 1000, got ${a.timeoutMs}`);
  if (!Number.isInteger(a.stallTtftMs) || a.stallTtftMs < 1) throw new Error(`--stall-ttft-ms must be a positive integer, got ${a.stallTtftMs}`);
  if (a.task && a.task !== "all" && !TASKS.includes(a.task)) {
    throw new Error(`--task must be one of ${TASKS.join("|")}|all, got ${a.task}`);
  }
  a.pir = a.pir ?? process.env.PIR_BIN ?? join(REPO, "target-alt", "release", "pir.exe");
  a.pi = a.pi ?? process.env.PI_CLI ?? join(process.env.APPDATA ?? "", "npm", "pi.cmd");
  return a;
}

function makeAgentDir(thinking = "low") {
  const real = join(process.env.USERPROFILE ?? process.env.HOME ?? "", ".pi", "agent");
  const dir = mkdtempSync(join(tmpdir(), "pir-hard-agent-"));
  try {
    for (const name of ["models.json", "settings.json", "auth.json"]) {
      const src = join(real, name);
      if (existsSync(src)) writeFileSync(join(dir, name), readFileSync(src));
    }
    const settingsPath = join(dir, "settings.json");
    try {
      const s = JSON.parse(readFileSync(settingsPath, "utf8"));
      delete s.packages;
      s.defaultThinkingLevel = thinking;
      writeFileSync(settingsPath, JSON.stringify(s, null, 2));
    } catch {
      writeFileSync(settingsPath, JSON.stringify({ defaultThinkingLevel: thinking }));
    }
    return dir;
  } catch (error) {
    rmSync(dir, { recursive: true, force: true });
    throw error;
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
    error: result.error ? String(result.error.message ?? result.error) : null,
  };
}

/** Resolve a generated `*-leg.cmd` launcher to its literal target, if any. */
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
  try {
    const stat = statSync(info.realpath);
    info.mtime = stat.mtime.toISOString();
    info.bytes = stat.size;
  } catch {}
  try { info.sha256 = hashFileSha256(info.realpath); } catch (error) { info.sha256 = null; info.hashError = String(error?.message ?? error); }
  const version = runBinaryVersion(info.realpath);
  info.version = version.version;
  info.versionExitCode = version.status;
  if (version.error) info.versionError = version.error;
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

/** Reject a release pir older than the tree it claims to represent. */
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

/**
 * Peak working set of the WHOLE PROCESS TREE, in MB, sampled every 3 s.
 * Sampling the spawned pid alone measures the cmd.exe shim on Windows (or the
 * node wrapper), not the agent; the tree sum is the fair instrument.
 */
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
        // The CIM parent/child graph can contain a cycle (a reused/inherited
        // ppid or a wrapper that points back into the tree); without a visited
        // set the walk grows `stack` forever and throws
        // `RangeError: Invalid array length`, which aborted a whole campaign.
        const visited = new Set();
        while (stack.length) {
          const cur = stack.pop();
          if (visited.has(cur)) continue;
          visited.add(cur);
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
        if (entry === "report.md" || entry === "report.json"
            || /^events-/.test(entry) || /^stderr-/.test(entry)
            || /-(?:pir|pi)-r\d+$/.test(entry)) {
          rmSync(join(resolved, entry), { recursive: true, force: true });
        }
      }
    }
  } else {
    mkdirSync(resolved, { recursive: true });
  }
}

function killTree(child, dir) {
  if (child.pid !== undefined) spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true });
  if (process.platform !== "win32") {
    try { child.kill("SIGKILL"); } catch {}
    return;
  }
  const q = dir.replace(/'/g, "''");
  const script = `$me=$PID; Get-CimInstance Win32_Process | Where-Object { $_.ProcessId -ne $me -and $_.CommandLine -and $_.CommandLine.Contains('${q}') } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }`;
  spawnSync("powershell", ["-NoProfile", "-Command", script], { windowsHide: true, timeout: 20000 });
}

function runLeg({ cli, label, work, prompt, model, agentDir, timeoutMs, tools = null }) {
  return new Promise((done) => {
    const isCmd = process.platform === "win32" && /\.(cmd|bat)$/i.test(cli);
    const program = isCmd ? process.env.ComSpec || "cmd.exe" : cli;
    // NEVER put prompt.md in argv on Windows: cmd.exe treats embedded
    // newlines as command separators and silently truncates the task to its
    // first line. Both CLIs join piped stdin with the initial message.
    const cliArgs = ["--mode", "json", "--print", "--model", model, "--no-skills", "--no-session",
      ...(tools ? ["--tools", tools] : [])];
    const args = isCmd ? ["/d", "/c", cli, ...cliArgs] : cliArgs;
    const started = Date.now();
    const child = spawn(program, args, {
      cwd: work,
      windowsHide: true,
      env: { ...process.env, PI_CODING_AGENT_DIR: agentDir, PIR_AGENT_DIR: agentDir },
      stdio: ["pipe", "pipe", "pipe"],
    });
    const rss = { peakMb: 0, timer: null };
    if (child.pid !== undefined) samplePeakTreeRssMb(child, rss);
    if (child.stdin) {
      child.stdin.on("error", () => { /* prompt delivery is checked via exit/verdict */ });
      try { child.stdin.end(prompt); } catch { /* exit/verdict reports a failed leg */ }
    }
    let buf = "";
    let firstLineAt = null;
    let requestAt = null;
    let ttftAt = null;
    const events = [];
    let err = "";
    let killed = false;
    let spawnError = null;
    let settled = false;
    let forceTimer = null;
    const finish = (code) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (forceTimer !== null) clearTimeout(forceTimer);
      if (rss.timer) clearInterval(rss.timer);
      let turns = 0, calls = 0, tokens = 0;
      let ttftFromRequestMs = null;
      for (const e of events) {
        if (e.type === "turn_start") turns++;
        if (e.type === "tool_execution_end") calls++;
        if (e.type === "message_end" && e.message?.role === "assistant") {
          const u = e.message.usage;
          if (u) tokens += u.totalTokens ?? ((u.input ?? 0) + (u.output ?? 0));
        }
      }
      if (ttftAt !== null && requestAt !== null && ttftAt >= requestAt) {
        ttftFromRequestMs = ttftAt - requestAt;
      }
      done({
        label, wallMs: Date.now() - started, startupMs: firstLineAt && firstLineAt - started,
        ttftMs: ttftAt && ttftAt - started, ttftFromRequestMs, turns, calls, tokens,
        peakRssMb: Math.round(rss.peakMb * 10) / 10,
        exitCode: code, timedOut: killed, spawnError, events, stderr: err.slice(0, 4000),
      });
    };
    const timer = setTimeout(() => {
      killed = true;
      killTree(child, work);
      forceTimer = setTimeout(() => {
        try { child.stdout?.destroy(); } catch {}
        try { child.stderr?.destroy(); } catch {}
        finish(null);
      }, 5_000);
    }, timeoutMs);
    child.stdout.on("data", (chunk) => {
      buf += chunk.toString("utf8");
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, i).trim();
        buf = buf.slice(i + 1);
        if (!line) continue;
        if (firstLineAt === null) firstLineAt = Date.now();
        let e;
        try { e = JSON.parse(line); } catch { e = { type: "_unparsed" }; }
        const kind = e.assistantEvent ?? e.assistantMessageEvent?.type ?? null;
        if (requestAt === null && e.type === "message_update" && kind === "start") requestAt = Date.now();
        if (ttftAt === null && e.type === "message_update" && /_delta$/.test(kind ?? "")) ttftAt = Date.now();
        e._at = Date.now() - started;
        events.push(e);
      }
    });
    child.stderr.on("data", (c) => { err += c.toString("utf8"); });
    child.on("error", (error) => {
      spawnError = String(error?.message ?? error);
      err += spawnError;
      finish(null);
    });
    child.on("close", (code) => finish(code));
  });
}

function verdict(task, work) {
  const script = join(TASKS_DIR, task, "hidden", "verdict.mjs");
  const r = spawnSync(process.execPath, [script, work], { encoding: "utf8", windowsHide: true, timeout: 300_000 });
  const out = `${r.stdout ?? ""}`.trim();
  // The verdict prints PRETTY-printed JSON (multi-line); parse the whole
  // output first, then fall back to the first `{` if a wrapper added noise.
  try {
    return JSON.parse(out);
  } catch {}
  const start = out.indexOf("{");
  if (start >= 0) {
    try { return JSON.parse(out.slice(start)); } catch {}
  }
  return { score: null, notes: [`verdict parse failed (exit ${r.status}): ${out.slice(0, 300)}`] };
}

function median(values) {
  const nums = values.filter((v) => Number.isFinite(v)).sort((a, b) => a - b);
  if (nums.length === 0) return null;
  const mid = Math.floor(nums.length / 2);
  return nums.length % 2 === 1 ? nums[mid] : Math.round((nums[mid - 1] + nums[mid]) / 2);
}

async function main() {
  const a = parseArgs(process.argv.slice(2));
  if (a.help) {
    process.stdout.write(`${USAGE}\n`);
    return;
  }
  const stamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
  // Per-process default: concurrent campaigns must not share (or --force-wipe)
  // one artifacts dir.
  const outDir = resolve(a.out ?? join(tmpdir(), "pir-harder", `${stamp}-${process.pid}`));
  const tasks = a.task && a.task !== "all" ? [a.task] : TASKS;

  // Dry run: validate args and print the plan. Resolves no binary, reads no
  // key, creates nothing, and needs no network — so it works before the
  // binaries exist and without a gateway.
  if (a.dryRun) {
    console.log("[harder] dry run — nothing is started, no binary is resolved");
    console.log(`[harder] tasks      ${tasks.join(", ")}`);
    console.log(`[harder] runs       ${a.runs} (per task, pir+pi interleaved)`);
    console.log(`[harder] model      ${a.model}`);
    console.log(`[harder] pir        ${a.pir}`);
    console.log(`[harder] pi         ${a.pi}`);
    console.log(`[harder] timeoutMs  ${a.timeoutMs}`);
    console.log(`[harder] out        ${outDir}`);
    console.log(`[harder] agent dir  fresh temp dir per leg (removed afterwards)`);
    console.log(`[harder] work dir   fresh temp dir per leg outside the repo (archived to out/)`);
    console.log(`[harder] stall rule TTFT > ${a.stallTtftMs} ms is not stall-free`);
    console.log(`[harder] score      passed/total hidden checks (quality signal)`);
    return;
  }

  if (!existsSync(a.pir)) throw new Error(`pir not found: ${a.pir}`);
  if (!existsSync(a.pi)) throw new Error(`pi not found: ${a.pi}`);
  const binaries = { pir: binaryInfo("pir", a.pir), pi: binaryInfo("pi", a.pi) };
  assertDistinctBinaries(binaries.pir, binaries.pi);
  // A stale release build makes the comparison meaningless; refuse it unless
  // the caller explicitly freezes the binary.
  const freshness = checkReleaseFreshness(binaries.pir, a.allowStale);
  // Only now — after every validation — touch the artifacts dir, so a refused
  // invocation leaves no empty directory behind.
  prepareOutDir(outDir, a.force);

  const rows = [];
  for (let rep = 1; rep <= a.runs; rep++) {
    for (const task of tasks) {
      const prompt = readFileSync(join(TASKS_DIR, task, "prompt.md"), "utf8");
      for (const [cli, label] of [[a.pir, "pir"], [a.pi, "pi"]]) {
        const work = mkdtempSync(join(tmpdir(), `pir-hard-${task}-${label}-`));
        let agentDir = null;
        try {
          cpSync(join(TASKS_DIR, task, "fixture"), work, { recursive: true });
          agentDir = makeAgentDir(a.thinking);
          console.log(`[harder] ${label} · ${task} · rep ${rep} …`);
          const run = await runLeg({ cli, label, work, prompt, model: a.model, agentDir, timeoutMs: a.timeoutMs, tools: a.tools });
          const v = verdict(task, work);
          const keep = join(outDir, `${task}-${label}-r${rep}`);
          try { cpSync(work, keep, { recursive: true }); } catch {}
          writeFileSync(join(outDir, `events-${task}-${label}-r${rep}.jsonl`), run.events.map((e) => JSON.stringify(e)).join("\n"));
          const row = {
            task, label, rep, score: v.score, passed: v.passed, total: v.total,
            wallMs: run.wallMs, startupMs: run.startupMs, ttftMs: run.ttftMs,
            ttftFromRequestMs: run.ttftFromRequestMs, peakRssMb: run.peakRssMb,
            turns: run.turns, calls: run.calls, tokens: run.tokens,
            exitCode: run.exitCode, timedOut: run.timedOut, spawnError: run.spawnError,
            notes: v.notes ?? [], binary: binaries[label],
            stallFree: !run.timedOut && !run.spawnError
              && Number.isFinite(run.ttftFromRequestMs ?? run.ttftMs)
              && (run.ttftFromRequestMs ?? run.ttftMs) <= a.stallTtftMs,
          };
          rows.push(row);
          console.log(`[harder] ${label} · ${task} → score ${v.score} (${v.passed}/${v.total}) · wall ${Math.round(run.wallMs / 1000)}s · turns ${run.turns} · tokens ${run.tokens} · rss ${run.peakRssMb}MB`);
        } finally {
          rmSync(work, { recursive: true, force: true });
          if (agentDir) rmSync(agentDir, { recursive: true, force: true });
        }
      }
    }
  }
  writeFileSync(join(outDir, "report.json"), JSON.stringify(rows, null, 2));
  const md = [
    "# Harder offline tasks (pir vs pi) — hidden-test quality scores",    "",
    `- time: ${new Date().toISOString()}`,
    `- model: \`${a.model}\``,
    `- pir: \`${binaries.pir.realpath ?? a.pir}\` sha256=${binaries.pir.sha256 ?? "—"} version=${JSON.stringify(binaries.pir.version ?? "—")}`,
    `- pi: \`${binaries.pi.realpath ?? a.pi}\` sha256=${binaries.pi.sha256 ?? "—"} version=${JSON.stringify(binaries.pi.version ?? "—")}`,
    `- stale check: ${freshness.checked ? (freshness.stale ? "STALE (allowed)" : "ok") : freshness.reason}`,
    `- score = passed/total hidden checks per task. Medians are per leg over stall-free runs only.`,
    "",
    "| task | CLI | score | passed/total | wall | TTFT | TTFT(req) | peak RSS | turns | tools | tokens | stall-free | timeout |",
    "|---|---|---|---|---|---|---|---|---|---|---|---|---|",
  ];
  for (const r of rows) {
    md.push(`| ${r.task} | ${r.label} | ${r.score} | ${r.passed}/${r.total} | ${Math.round(r.wallMs / 1000)}s | ${r.ttftMs ? Math.round(r.ttftMs) + "ms" : "—"} | ${r.ttftFromRequestMs ? Math.round(r.ttftFromRequestMs) + "ms" : "—"} | ${r.peakRssMb || "—"}MB | ${r.turns} | ${r.calls} | ${r.tokens} | ${r.stallFree ? "yes" : "no"} | ${r.timedOut ? "YES" : ""} |`);
  }
  md.push("");
  writeFileSync(join(outDir, "report.md"), md.join("\n") + "\n");

  const totals = {};
  for (const r of rows) {
    const k = r.label;
    (totals[k] ??= { score: 0, n: 0, missing: 0, passed: 0, total: 0 });
    if (typeof r.score === "number" && Number.isFinite(r.score)) {
      totals[k].score += r.score;
      totals[k].passed += Number.isFinite(r.passed) ? r.passed : 0;
      totals[k].total += Number.isFinite(r.total) ? r.total : 0;
      totals[k].n++;
    } else {
      totals[k].missing++;
    }
  }
  for (const [k, t] of Object.entries(totals)) {
    const mean = t.n > 0 ? (t.score / t.n).toFixed(3) : "n/a";
    console.log(`[harder] ${k}: mean score ${mean} over ${t.n} scored runs (${t.passed}/${t.total} checks)` + (t.missing ? ` (${t.missing} verdict failure(s) excluded)` : ""));
  }

  // Machine-readable summary beside the report: per-leg hidden-test score plus
  // per-leg timing medians over stall-free runs only.
  const summary = {
    generatedAt: new Date().toISOString(),
    kind: "hidden-test-quality",
    model: a.model,
    outDir: outDir.replace(/\\/g, "/"),
    binaries: {
      pir: { realpath: binaries.pir.realpath ?? null, sha256: binaries.pir.sha256 ?? null, version: binaries.pir.version ?? null },
      pi: { realpath: binaries.pi.realpath ?? null, sha256: binaries.pi.sha256 ?? null, version: binaries.pi.version ?? null },
      freshness,
    },
    stallRule: { ttftMs: a.stallTtftMs },
    legs: Object.fromEntries(Object.entries(totals).map(([label, t]) => {
      const mine = rows.filter((r) => r.label === label);
      const kept = mine.filter((r) => r.stallFree && Number.isFinite(r.score));
      const m = (key) => median(kept.map((r) => r[key]));
      const droppedReasons = mine.filter((r) => !r.stallFree || !Number.isFinite(r.score)).map((r) => {
        const reasons = [];
        if (r.timedOut) reasons.push("timedOut");
        if (r.spawnError) reasons.push("spawnError");
        const ttft = Number.isFinite(r.ttftFromRequestMs) ? r.ttftFromRequestMs : r.ttftMs;
        if (!Number.isFinite(ttft)) reasons.push("ttft-missing");
        else if (ttft > a.stallTtftMs) reasons.push(`ttft>${a.stallTtftMs}ms`);
        if (!Number.isFinite(r.score)) reasons.push("unscored");
        return { run: r.rep, task: r.task, reasons };
      });
      return [label, {
        runs: mine.length,
        kept: kept.length,
        dropped: mine.length - kept.length,
        droppedReasons,
        score: {
          mean: t.n > 0 ? Number((t.score / t.n).toFixed(4)) : null,
          passedChecks: t.passed,
          totalChecks: t.total,
          scoredRuns: t.n,
          unscoredRuns: t.missing,
          fullMarks: mine.filter((r) => Number.isFinite(r.total) && r.score === r.total).length,
        },
        median: {
          wallMs: m("wallMs"),
          startupMs: m("startupMs"),
          ttftMs: m("ttftMs"),
          ttftFromRequestMs: m("ttftFromRequestMs"),
          peakRssMb: m("peakRssMb"),
          turns: m("turns"),
          totalTokens: m("tokens"),
        },
      }];
    })),
  };
  summary.comparable = Object.keys(summary.legs).length === 2
    && Object.values(summary.legs).every((leg) => leg.kept > 0);
  writeFileSync(join(outDir, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`);
  console.log(`\n[harder] report: ${join(outDir, "report.md")}`);
  console.log(`[harder] summary: ${join(outDir, "summary.json")} (medians over stall-free runs)`);

  const spawnFailed = rows.some((row) => row.spawnError);
  const incomplete = rows.some((row) => row.timedOut
    || !Number.isFinite(row.score)
    || !Number.isFinite(row.total)
    // `score` is a FRACTION (passed/total), so compare it with 1, not with
    // the check count: the old `score < total` made every non-empty task look
    // incomplete and forced exit 3 on a fully passing campaign.
    || row.passed < row.total);
  // 2 = spawn/harness error; 3 = any task below full score or timed out.
  process.exitCode = spawnFailed ? 2 : (incomplete ? 3 : 0);
}

main().catch((e) => {
  const message = String(e?.message ?? e);
  if (/unknown arg|needs a value|must be|not found/i.test(message)) {
    console.error(`[harder] ${message}`);
    process.exitCode = 1;
    return;
  }
  console.error(e);
  process.exitCode = 2;
});
