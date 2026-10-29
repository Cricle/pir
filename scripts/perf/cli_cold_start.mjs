// cli_cold_start.mjs — CLI cold-start timing for pir (PERF-SCOPE P2, item 2),
// with the optional upstream-pi control rows.
//
// Rows (each one fresh process per sample, spawned N times, sequential):
//   baseline    <node> --version                  harness floor: this script's
//                                                 own spawn+wait overhead
//   pir version <pir> --version                   exec → arg parse → exit
//   pir js      <pir> js -e "1+1"                 exec → QuickJS + node-compat
//                                                 runtime init → eval → exit
//   pir env-daemon <pir> env-daemon serve --token <hex>
//                                                 exec → early argv dispatch →
//                                                 daemon init → `PI-ENV <token>`
//                                                 sync line → stdin EOF → exit
//   pir print   <pir> print --no-session "..."    exec → config/extensions →
//                                                 session → one model turn
//   pi version  <pi> --version                    upstream control (node boot)
//   pi print    <pi> --no-session -p "reply OK"   upstream print-mode control
//
// Not measured (never guessed): there is no `pi js -e` control — upstream pi's
// `-e` is `--extension <path>` (packages/coding-agent/src/cli/args.ts), not an
// eval runner, so the pir `js -e` row has no counterpart. The row is reported
// as skipped with that note.
//
// Endpoint plumbing mirrors tests/agent_benchmark.rs: when the endpoint is
// fully specified, a temp agent dir holding the same `models.json` shape is
// generated and exported as PIR_AGENT_DIR (pir) and PI_CODING_AGENT_DIR (pi),
// and both CLIs get --provider/--model.
//
// "Fully specified" is the full PIR_BENCH_BASE_URL + PIR_BENCH_API_KEY +
// PIR_BENCH_MODEL trio — deliberately stricter than tests/agent_benchmark.rs,
// which falls back to a localhost ollama. Here a partial endpoint would cost
// one --timeout-ms per sample on a connect failure the caller never asked for,
// so any missing variable SKIPS both print rows with a note naming it, and the
// local-only rows still run. PIR_BENCH_PROVIDER is not required: it may
// default to "ollama" because only the file/flag agreement matters, not the id
// (same reasoning as tests/agent_benchmark.rs).
//
// Isolation: every row runs with cwd = an empty temp work dir and the temp
// agent dir above, so the repo's `.pi` project config and the machine's real
// ~/.pi (extensions, MCP servers, settings) are NOT loaded. Numbers are
// therefore fresh-install cold start, identical on both CLIs.
//
// Timing: process.hrtime.bigint() around spawn→close, so each sample INCLUDES
// this harness's spawn+wait cost — read the `baseline` row to size that
// subtraction. No warmup sample is discarded: min is the warm-cache best case,
// p90 the cold-ish tail. Per row the table reports ok/failed/timed-out counts
// plus min/median/p90 over the successful samples (ms), and the raw samples.
//
// Read-only wrt the repo: the only writes are the temp tree above
// (<os.tmpdir()>/pir-cli-cold-start-<pid>/, removed on exit) and the JSON report
// beside it (<os.tmpdir()>/pir-cli-cold-start-report-<pid>.json, kept — it is the
// machine-readable artifact). The prompt is a no-op ("reply OK") and print
// rows run with --no-session.
//
// Usage:
//   node scripts/perf/cli_cold_start.mjs [--pir <path>] [--pi <path>]
//                                        [--runs <N>] [--timeout-ms <MS>]
// Defaults: --pir = $PIR_EXE then target-alt/debug/pir.exe (then the repo's
// other candidates), --runs 10, --timeout-ms 120000 (print rows wait on a
// model round trip). Exit code 0 even when rows fail or skip — the table is
// the artifact; a missing --pir binary is the only hard error.
//
// Env: the two provider rows run only when PIR_BENCH_BASE_URL,
// PIR_BENCH_API_KEY and PIR_BENCH_MODEL are all set (see above); anything
// missing is reported as a skip. --pi may be an executable path or a bare PATH
// name; without it PIR_BENCH_PI_BIN, then `pi.cmd`/`pi.exe`/`pi` are tried in
// order, and with none runnable the control rows are reported as skipped.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { StringDecoder } from "node:string_decoder";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
// The system temp dir (os.tmpdir(): %TEMP% on Windows, $TMPDIR on Unix). Not
// `process.env.TEMP || process.env.TMP || "."`: on Unix neither TEMP nor TMP is
// normally set, and "." would drop the work dir — the cwd every sampled CLI
// runs in — and the report into the repo, breaking the isolation and the
// read-only-wrt-repo contract below.
const TEMP_ROOT = join(tmpdir(), `pir-cli-cold-start-${process.pid}`);
const WORK_DIR = join(TEMP_ROOT, "work");
const AGENT_DIR = join(TEMP_ROOT, "agent");

function argValue(name) {
  const i = process.argv.indexOf(name);
  if (i !== -1 && process.argv[i + 1] && !process.argv[i + 1].startsWith("--")) {
    return process.argv[i + 1];
  }
  const prefix = `${name}=`;
  const inline = process.argv.find((arg) => arg.startsWith(prefix));
  return inline ? inline.slice(prefix.length) : null;
}

function positiveInt(value, fallback) {
  const parsed = Number.parseInt(value ?? "", 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

function findPir() {
  const explicit = argValue("--pir");
  if (explicit) return resolve(explicit);
  for (const candidate of [
    process.env.PIR_EXE,
    // debug first, same order as scripts/eco_census.mjs: it is the build the
    // eval loops use.
    join(REPO, "target-alt", "debug", "pir.exe"),
    join(REPO, "target-alt", "release", "pir.exe"),
    join(REPO, "target", "debug", "pir.exe"),
    join(REPO, "target", "release", "pir.exe"),
  ].filter(Boolean)) {
    if (existsSync(candidate)) return candidate;
  }
  return null;
}

const PIR = findPir();
if (!PIR) {
  console.error(
    "error: pir binary not found — build it or pass --pir <path> " +
      "(e.g. --pir target-alt/debug/pir.exe)",
  );
  process.exit(1);
}
// An explicit path is taken as given; a typo must fail loudly here rather than
// degrade into "every row spawn-error" (the one hard error this script has).
if (!existsSync(PIR)) {
  console.error(`error: --pir ${PIR} does not exist`);
  process.exit(1);
}

const RUNS = positiveInt(argValue("--runs"), 10);
const TIMEOUT_MS = positiveInt(argValue("--timeout-ms"), 120_000);
// After the timeout's tree kill, how long to wait for the child's `close`
// before force-resolving the sample as timed out (see `runOnce`).
const KILL_GRACE_MS = 5_000;

const PI_ARG = argValue("--pi") || process.env.PIR_BENCH_PI_BIN || "";
// An explicit --pi / PIR_BENCH_PI_BIN is used verbatim, never second-guessed.
// Otherwise probe the plausible PATH spellings: upstream's launcher is `pi`,
// `pi.cmd` is what node can actually exec without a shell, and `pi.exe` covers
// a native install. The first candidate that answers `--version` wins, so a
// missing install degrades to a skip note instead of a false "not runnable".
const PI_EXPLICIT = PI_ARG
  ? PI_ARG.includes("/") || PI_ARG.includes("\\")
    ? resolve(PI_ARG)
    : PI_ARG
  : "";
const PI_CANDIDATES = PI_EXPLICIT
  ? [PI_EXPLICIT]
  : process.platform === "win32"
    ? ["pi.cmd", "pi.exe", "pi"]
    : ["pi"];

const BENCH_BASE_URL = (process.env.PIR_BENCH_BASE_URL || "").trim().replace(/\/+$/, "");
const BENCH_API_KEY = (process.env.PIR_BENCH_API_KEY || "").trim();
const BENCH_MODEL = (process.env.PIR_BENCH_MODEL || "").trim();
const BENCH_PROVIDER = (process.env.PIR_BENCH_PROVIDER || "").trim() || "ollama";
const PROMPT = "reply OK";
// Fixed sync-line token for the env-daemon row. The daemon echoes whatever
// `--token` it is given into its `PI-ENV <token>` line, so any string works; a
// fixed hex one keeps the row's command reproducible (same shape the daemon's
// clients use).
const DAEMON_TOKEN = "0123456789abcdef0123456789abcdef";

// The provider rows run only on a complete endpoint (see the header): a
// partial one would spend a timeout per sample on a connect failure.
const MISSING_BENCH_ENV = [
  ["PIR_BENCH_BASE_URL", BENCH_BASE_URL],
  ["PIR_BENCH_API_KEY", BENCH_API_KEY],
  ["PIR_BENCH_MODEL", BENCH_MODEL],
]
  .filter(([, value]) => value === "")
  .map(([name]) => name);
const PRINT_READY = MISSING_BENCH_ENV.length === 0;

// ---------------------------------------------------------------------------
// Fixtures (temp only; removed in the finally block below)
// ---------------------------------------------------------------------------

/**
 * The models.json both CLIs read — same shape tests/agent_benchmark.rs writes
 * (an OpenAI-compatible provider with an inline model entry). Materialized
 * only when the env trio is complete (see the header); otherwise the agent
 * dir stays empty (fresh-install config).
 */
function modelsJson() {
  return JSON.stringify(
    {
      providers: {
        [BENCH_PROVIDER]: {
          name: `cli cold start (${BENCH_PROVIDER})`,
          baseUrl: BENCH_BASE_URL,
          apiKey: BENCH_API_KEY,
          api: "openai-completions",
          models: [
            {
              id: BENCH_MODEL,
              name: `cold start ${BENCH_MODEL}`,
              api: "openai-completions",
              baseUrl: BENCH_BASE_URL,
              reasoning: false,
              contextWindow: 32768,
              maxTokens: 8192,
              input: ["text"],
            },
          ],
        },
      },
    },
    null,
    2,
  );
}

mkdirSync(WORK_DIR, { recursive: true });

// ---------------------------------------------------------------------------
// Spawning: node refuses .cmd/.bat without a shell (EINVAL since Node 20.12),
// so those go through cmd.exe explicitly rather than `shell: true` string
// munging. Everything else is spawned directly, never through a shell.
// ---------------------------------------------------------------------------

function quoteForCmd(part) {
  return /[\s"&|<>^()]/.test(part) ? `"${part.replace(/"/g, '""')}"` : part;
}

function spawnCommand(program, args, options) {
  if (process.platform === "win32" && /\.(cmd|bat)$/i.test(program)) {
    const comspec = process.env.ComSpec || process.env.COMSPEC || "cmd.exe";
    // ARGV form, not a single `"<line>"` string: `cmd /d /s /c "<prog>" args`
    // fails with "filename/directory syntax is incorrect" for an npm shim path
    // (measured 2026-10-16: rc=1), while passing the program and args as
    // separate argv entries lets Node's own Windows quoting do the right thing
    // (rc=0, `pi --version` -> 1.1.0).
    return spawn(comspec, ["/d", "/c", program, ...args], options);
  }
  return spawn(program, args, options);
}

function killTree(child) {
  if (!child.pid) return;
  if (process.platform === "win32") {
    try {
      spawnSync("taskkill", ["/pid", String(child.pid), "/t", "/f"], { stdio: "ignore" });
    } catch {
      // taskkill missing/denied: the direct kill below is the fallback.
    }
  }
  try {
    child.kill("SIGKILL");
  } catch {
    // Already gone.
  }
}

function runOnce(item) {
  return new Promise((resolvePromise) => {
    const started = process.hrtime.bigint();
    let child;
    try {
      child = spawnCommand(item.program, item.args, {
        cwd: WORK_DIR,
        env: item.env,
        stdio: ["ignore", "pipe", "pipe"],
        windowsHide: true,
      });
    } catch (error) {
      resolvePromise({ ms: 0, error, stdout: "", stderr: "" });
      return;
    }
    let stdout = "";
    let stderr = "";
    let settled = false;
    let timedOut = false;
    let settleTimer = null;
    const timer = setTimeout(() => {
      timedOut = true;
      killTree(child);
      // Second-chance fallback: if neither taskkill nor SIGKILL is honored (or
      // a grandchild keeps the pipes open past the tree kill), do not hang the
      // whole run on one sample — resolve it as timed out and move on.
      settleTimer = setTimeout(() => {
        process.stderr.write(
          `warning: ${item.program} did not exit ${KILL_GRACE_MS} ms after the kill; ` +
            "counting the sample as timed out\n",
        );
        finish({ code: null, signal: null });
      }, KILL_GRACE_MS);
    }, TIMEOUT_MS);
    // Chunks split multi-byte UTF-8 sequences; plain `+=` on a Buffer string
    // would corrupt non-ASCII output (the first-output sanity check below).
    const stdoutDecoder = new StringDecoder("utf8");
    const stderrDecoder = new StringDecoder("utf8");
    const finish = (result) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (settleTimer !== null) clearTimeout(settleTimer);
      stdout += stdoutDecoder.end();
      stderr += stderrDecoder.end();
      const ms = Number(process.hrtime.bigint() - started) / 1e6;
      resolvePromise({ ms, timedOut, stdout, stderr, ...result });
    };
    child.stdout.on("data", (chunk) => {
      stdout += stdoutDecoder.write(chunk);
    });
    child.stderr.on("data", (chunk) => {
      stderr += stderrDecoder.write(chunk);
    });
    child.on("error", (error) => finish({ error }));
    child.on("close", (code, signal) => finish({ code, signal }));
  });
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

const agentEnv = {
  ...process.env,
  PIR_AGENT_DIR: AGENT_DIR,
  PI_CODING_AGENT_DIR: AGENT_DIR,
  NO_COLOR: "1",
};

const items = [];
const skipped = [];

items.push({
  id: "baseline",
  label: "baseline: node --version",
  program: process.execPath,
  args: ["--version"],
  env: agentEnv,
  note: "harness spawn+wait floor (subtract from the rows below)",
});

items.push({
  id: "pir --version",
  label: "pir --version",
  program: PIR,
  args: ["--version"],
  env: agentEnv,
  note: "exec → clap parse → exit",
});

items.push({
  id: "pir js -e",
  label: 'pir js -e "1+1"',
  program: PIR,
  args: ["js", "-e", "1+1"],
  env: agentEnv,
  note: "exec → QuickJS + node-compat runtime init → eval → exit",
});

// `pir env-daemon` (PERF-SCOPE P2): intercepted on the raw argv before clap,
// config and extension loading (main_cli/run_mode.rs:29), so it is the
// leanest real subcommand to time. Like every row here stdin is the null
// device, so the daemon writes its `PI-ENV <token>` sync line, reads EOF right
// after, and exits 0 — the same clean lifecycle a real client ends with (the
// client's ready-line read is exercised by tests/env_daemon_protocol.rs, not
// timed here). A sample that exits 0 must have emitted the sync line first (a
// failed write exits 1), so the row cannot silently pass without a ready
// daemon; the first-output block below shows the line.
items.push({
  id: "pir env-daemon",
  label: `pir env-daemon serve --token ${DAEMON_TOKEN}`,
  program: PIR,
  args: ["env-daemon", "serve", "--token", DAEMON_TOKEN],
  env: agentEnv,
  note: "exec → early argv dispatch → daemon init → PI-ENV sync line → stdin EOF → clean exit",
});

const pirPrintArgs = [
  "print",
  "--no-session",
  "--provider",
  BENCH_PROVIDER,
  "--model",
  BENCH_MODEL,
  PROMPT,
];
if (PRINT_READY === false) {
  skipped.push({
    id: "pir print --no-session",
    note:
      `provider rows need all of PIR_BENCH_BASE_URL/PIR_BENCH_API_KEY/PIR_BENCH_MODEL; ` +
      `missing: ${MISSING_BENCH_ENV.join(", ")} (local rows still ran)`,
  });
} else {
  items.push({
    id: "pir print --no-session",
    label: 'pir print --no-session "reply OK"',
    program: PIR,
    args: pirPrintArgs,
    env: agentEnv,
    note: `exec → config/extensions → session → one model turn (${BENCH_MODEL})`,
  });
}

// ---------------------------------------------------------------------------
// Stats + reporting
// ---------------------------------------------------------------------------

function sortedSuccessful(samples) {
  return samples
    .filter((sample) => !sample.timedOut && !sample.error && sample.code === 0)
    .map((sample) => sample.ms)
    .sort((a, b) => a - b);
}

function median(sorted) {
  if (sorted.length === 0) return Number.NaN;
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 1
    ? sorted[middle]
    : (sorted[middle - 1] + sorted[middle]) / 2;
}

function p90(sorted) {
  if (sorted.length === 0) return Number.NaN;
  const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil(0.9 * sorted.length) - 1));
  return sorted[index];
}

function ms(value) {
  return Number.isNaN(value) ? "n/a" : value.toFixed(1);
}

function pad(value, width) {
  return String(value).padEnd(width);
}

function padStart(value, width) {
  return String(value).padStart(width);
}

function shortCommand(item) {
  const quoted = item.args.map((arg) => (/[\s"]/.test(arg) ? JSON.stringify(arg) : arg));
  return `${JSON.stringify(item.program)} ${quoted.join(" ")}`;
}

async function runItem(item) {
  const samples = [];
  for (let run = 0; run < RUNS; run += 1) {
    samples.push(await runOnce(item));
  }
  const successful = sortedSuccessful(samples);
  return {
    item,
    samples,
    successful,
    timedOut: samples.filter((sample) => sample.timedOut).length,
    spawnErrors: samples.filter((sample) => sample.error).length,
    failed: samples.filter((sample) => !sample.timedOut && !sample.error && sample.code !== 0)
      .length,
    firstOutput: (samples.find((sample) => sample.stdout && sample.stdout.trim()) || {}).stdout,
    firstStderr: (samples.find((sample) => sample.stderr && sample.stderr.trim()) || {}).stderr,
  };
}

/**
 * Best-effort build-profile label from the binary path. Cargo's layout puts the
 * binary in `<target-dir>/{debug,release}/`, so the immediate parent directory
 * is authoritative. A path outside that layout has no authoritative profile:
 * segments are still scanned, but the result is labeled "inferred" rather than
 * silently reported as fact (the old substring heuristic labeled, say,
 * C:\builds\pir.exe "debug" purely because the path lacked "release").
 */
function describeBuild(program) {
  const parts = [];
  if (typeof program === "string" && existsSync(program)) {
    const stats = statSync(program);
    const segments = program.toLowerCase().split(/[\\/]+/).filter(Boolean);
    const isProfileDir = (name) => name === "debug" || name === "release";
    const parent = segments.length >= 2 ? segments[segments.length - 2] : "";
    let profile = isProfileDir(parent) ? parent : null;
    let inferred = false;
    if (profile === null) {
      profile = segments.slice(0, -1).find(isProfileDir) ?? null;
      inferred = profile !== null;
    }
    const alt = segments.includes("target-alt") ? ", target-alt" : "";
    if (profile === null) {
      parts.push("build profile unknown, path not under a debug/release dir");
    } else {
      parts.push(`${profile}${inferred ? " (inferred from path)" : ""}${alt} build`);
    }
    parts.push(`${(stats.size / (1024 * 1024)).toFixed(1)} MiB`);
    parts.push(`mtime ${stats.mtime.toISOString()}`);
  }
  return parts.join(", ");
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

const results = [];
try {
  if (PRINT_READY === true) {
    mkdirSync(AGENT_DIR, { recursive: true });
    writeFileSync(join(AGENT_DIR, "models.json"), modelsJson());
  }

  // First candidate spelling that answers `--version` wins; each miss is just
  // the next probe, and an all-miss is the skip note below.
  let PI = null;
  for (const candidate of PI_CANDIDATES) {
    const probe = await runOnce({ program: candidate, args: ["--version"], env: agentEnv });
    if (!probe.error && probe.code === 0) {
      PI = candidate;
      break;
    }
  }
  const piOk = PI !== null;
  if (!piOk) {
    skipped.push({
      id: "pi controls",
      note:
        `no upstream pi answered --version (tried: ${PI_CANDIDATES.join(", ")}); ` +
        "point --pi / PIR_BENCH_PI_BIN at the executable",
    });
  } else {
    items.push({
      id: "pi --version",
      label: "pi --version",
      program: PI,
      args: ["--version"],
      env: agentEnv,
      note: "upstream control: node boot + arg parse → exit",
    });
    if (PRINT_READY === false) {
      skipped.push({
        id: "pi --no-session -p",
        note: `provider rows disabled; missing: ${MISSING_BENCH_ENV.join(", ")}`,
      });
    } else {
      items.push({
        id: "pi --no-session -p",
        label: 'pi --no-session -p "reply OK"',
        program: PI,
        args: ["--no-session", "--provider", BENCH_PROVIDER, "--model", BENCH_MODEL, "-p", PROMPT],
        env: agentEnv,
        note: `exec → node boot → config → one model turn (${BENCH_MODEL})`,
      });
    }
  }
  // No `pi js -e` row by design: upstream pi's -e is --extension <path>
  // (packages/coding-agent/src/cli/args.ts), not an eval runner.
  skipped.push({
    id: "pi js -e",
    note: "no upstream equivalent: pi's -e is --extension <path>, not an eval runner",
  });

  for (const item of items) {
    process.stdout.write(`running ${item.label} x${RUNS} ... `);
    const result = await runItem(item);
    results.push(result);
    process.stdout.write("done\n");
  }

  console.log("");
  console.log("=== pir CLI cold start ===");
  console.log(
    `date:    ${new Date().toISOString()}    node: ${process.version}    platform: ${process.platform} ${process.arch}`,
  );
  const pirBuild = describeBuild(PIR);
  console.log(`pir:     ${PIR}${pirBuild ? `    (${pirBuild})` : ""}`);
  console.log(
    `pi:      ${piOk ? PI : "(not runnable / not used)"}    runs: ${RUNS}    timeout: ${TIMEOUT_MS} ms    cwd: ${WORK_DIR}`,
  );
  console.log(
    `agent:   ${
      PRINT_READY === false
        ? `empty temp agent dir, no provider configured (missing: ${MISSING_BENCH_ENV.join(", ")})`
        : `model ${BENCH_MODEL} via ${BENCH_BASE_URL} (provider ${BENCH_PROVIDER}, api key set)`
    }`,
  );
  console.log(`build:   see the per-binary profile above; all rows are fresh processes`);
  console.log("");
  console.log(
    `${pad("item", 30)}${padStart("n", 3)}${padStart("ok", 4)}${padStart("fail", 6)}${padStart("to", 4)}` +
      `${padStart("min", 10)}${padStart("median", 10)}${padStart("p90", 10)}  note`,
  );
  // The `to` column is the timed-out + spawn-error count, not a second exit
  // status (the JSON report's `timeoutOrSpawnError`); spell the columns out
  // once so the header abbreviation cannot be misread.
  console.log(
    "columns: n samples; ok = exited 0; fail = exited non-zero; to = timed out + spawn error; " +
      "min/median/p90 over ok samples (ms)",
  );
  console.log("-".repeat(120));
  for (const result of results) {
    const { item, successful } = result;
    console.log(
      `${pad(item.id, 30)}${padStart(result.samples.length, 3)}${padStart(successful.length, 4)}` +
        `${padStart(result.failed, 6)}${padStart(result.timedOut + result.spawnErrors, 4)}` +
        `${padStart(ms(successful.length ? successful[0] : Number.NaN), 10)}` +
        `${padStart(ms(median(successful)), 10)}` +
        `${padStart(ms(p90(successful)), 10)}  ${item.note}`,
    );
  }
  for (const entry of skipped) {
    console.log(`${pad(entry.id, 30)}${padStart("skipped", 17)}  ${entry.note}`);
  }

  console.log("");
  console.log("commands:");
  for (const result of results) {
    console.log(`  ${pad(result.item.id, 28)} ${shortCommand(result.item)}`);
  }

  console.log("");
  console.log("samples (ms, X = timeout/failure):");
  for (const result of results) {
    const cells = result.samples.map((sample) =>
      sample.timedOut
        ? "X"
        : sample.error || sample.code !== 0
          ? "E"
          : sample.ms.toFixed(1),
    );
    console.log(`  ${pad(result.item.id, 28)} ${cells.join(" ")}`);
  }

  console.log("");
  console.log("first successful output (sanity check that the row really ran):");
  for (const result of results) {
    const first = (result.firstOutput || result.firstStderr || "(no output)")
      .trim()
      .split(/\r?\n/)[0];
    // Codepoint-safe clamp: a UTF-16 `slice` could cut a surrogate pair in half.
    const line = [...first].slice(0, 100).join("");
    console.log(`  ${pad(result.item.id, 28)} ${line || "(empty)"}`);
  }

  const report = {
    date: new Date().toISOString(),
    node: process.version,
    platform: `${process.platform} ${process.arch}`,
    pir: PIR,
    pirBuild: describeBuild(PIR),
    pi: piOk ? PI : null,
    runs: RUNS,
    timeoutMs: TIMEOUT_MS,
    endpoint: PRINT_READY === false ? null : BENCH_BASE_URL,
    model: PRINT_READY === false ? null : BENCH_MODEL,
    rows: results.map((result) => ({
      id: result.item.id,
      command: shortCommand(result.item),
      samples: result.samples.length,
      ok: result.successful.length,
      failed: result.failed,
      timeoutOrSpawnError: result.timedOut + result.spawnErrors,
      minMs: result.successful.length ? result.successful[0] : null,
      medianMs: result.successful.length ? median(result.successful) : null,
      p90Ms: result.successful.length ? p90(result.successful) : null,
      samplesMs: result.samples.map((sample) =>
        sample.timedOut || sample.error || sample.code !== 0 ? null : sample.ms,
      ),
    })),
    skipped,
  };
  // Deliberately NOT under TEMP_ROOT: the `finally` below removes that tree,
  // so a report written inside it would be deleted before the caller could
  // read the path just printed. This one sits beside it in the system temp dir.
  const reportPath = join(
    tmpdir(),
    `pir-cli-cold-start-report-${process.pid}.json`,
  );
  writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  console.log("");
  console.log(`json report: ${reportPath}`);
} finally {
  try {
    rmSync(TEMP_ROOT, { recursive: true, force: true });
  } catch {
    // Best effort: a lingering temp tree must not fail the run.
  }
}
