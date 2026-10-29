#!/usr/bin/env node
/**
 * Reproducible A/B campaign: `pir` vs upstream `pi` against the `ar` gateway
 * with the `gpt-5` model, using an isolated agent-dir template
 * that reads the credential from `$PIR_AB_API_KEY` (models.json `$ENV`
 * template, resolved by the CLI at request time -?the key is never written to
 * disk and never printed).
 *
 * Why this wrapper exists (instead of calling scripts/perf/ab_bird.mjs
 * directly):
 *  - ab_bird copies models.json from the *real* `~/.pi/agent`, so a run cannot
 *    be pinned to one provider/model without changing the user's config;
 *  - it creates one temp agent dir shared by both legs. This wrapper supplies
 *    TWO persistent isolated agent dirs (pir/pi) via tiny per-leg launchers,
 *    and points the harness process' HOME/USERPROFILE at a throwaway home, so
 *    neither the real `~/.pi` nor pi's `~/.agents/skills` is ever read;
 *  - it runs the gateway-stall canary (probe wall > 8 s or TTFT > 10 s -?dirty
 *    window) before and after a scored campaign.
 *
 * Usage:
 *   export PIR_AB_API_KEY=...
 *   node scripts/perf/ab-ar/run_ab.mjs --task probe --runs 3 \
 *       --pir target-alt/release/pir.exe --pi "$APPDATA/npm/pi.cmd"
 *   node scripts/perf/ab-ar/run_ab.mjs --task bird --runs 2 --p3
 *
 * Flags:
 *   --task bird|app|probe|gen|quality
 *                           ab_bird task (default bird). bird/app are
 *                           ARTIFACT/self-eval checks, NOT quality scores.
 *                           `quality` routes to harder_tasks_campaign.mjs
 *                           (hidden-test scores).
 *   --campaign ar|openai     gateway preset (default ar); the template supplies
 *                           provider/model/api/authHeader/baseUrl
 *   --template <path>       explicit models.json template (overrides the path)
 *   --runs N                repetitions (default 1; ab_bird interleaves legs)
 *   --pir <exe>             pir binary (default: target-alt/release/pir.exe)
 *   --pi <cmd>              upstream pi entry point (default: %APPDATA%/npm/pi.cmd)
 *   --model <provider/id>   pinned to the selected template's model
 *   --endpoint <url>        override baseUrl written to the runtime copies
 *   --thinking <level>      passed to ab_bird and settings (default low)
 *   --timeout-ms N          per-run timeout (default 900000)
 *   --out <dir>             artifacts dir (default $TMP/pir-ab-ar/<stamp>-<task>; outside repo)
 *   --quality-task <name>   harder_tasks_campaign task (default: all 5)
 *   --probe-legs both|pir|pi  legs per probe repetition (default both)
 *   --p3                    also run scripts/perf/p3_task_e2e.mjs
 *   --p3-task bug|feature|both  p3 task set (default both)
 *   --no-canary             skip the pre/post gateway-stall canary
 *   --allow-dirty           continue when the canary says DIRTY (tagged in meta)
 *   --dry-run               validate + print commands; starts no model calls
 *
 * Exit codes: 0 ok | 1 harness/config error | 2 dirty window (refused to score) -
 * 3 model/artifact verdict FAIL (the harness ran; inspect report.md/report.json).
 *
 * Node stdlib only. Never commit the API key.
 */

import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, readdirSync, readFileSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..", "..");
const AB_BIRD_PATH = join(REPO, "scripts", "perf", "ab_bird.mjs");
const P3_PATH = join(REPO, "scripts", "perf", "p3_task_e2e.mjs");
const QUALITY_PATH = join(REPO, "scripts", "perf", "harder_tasks_campaign.mjs");
const IS_WINDOWS = process.platform === "win32";

const KEY_ENV = "PIR_AB_API_KEY";

// Campaigns are just committed agent-dir templates. `ar` stays the default so
// every existing command keeps its exact meaning; a campaign is selected with
// `--campaign <name>` (= `agent-template-<name>/models.json`) or `--template
// <path>`. provider/model/api/baseUrl are derived from the template itself, so
// adding a gateway is a data change, not a runner change.
const DEFAULT_CAMPAIGN = "ar";
const CAMPAIGNS = {
  ar: {
    templatePath: join(HERE, "agent-template", "models.json"),
    baseUrl: "https://ai.1k0.com/v1",
    api: "openai-completions",
    authHeader: false,
  },
  openai: {
    templatePath: join(HERE, "agent-template-openai", "models.json"),
    baseUrl: "https://api.openai.com",
    api: "anthropic-messages",
    authHeader: true,
  },
};

/**
 * Resolve the selected campaign into a template path + the shape contract the
 * template must satisfy. `--template <path>` keeps the selected campaign's
 * `api`/`authHeader` contract but takes baseUrl from the file itself (a custom
 * template is its own endpoint); the committed campaigns pin baseUrl too, so a
 * typo in a checked-in template is a hard error rather than a silent re-target.
 */
function resolveCampaign(args) {
  const name = args.campaign ?? DEFAULT_CAMPAIGN;
  const known = CAMPAIGNS[name];
  if (!known) {
    throw new Error(`unknown --campaign "${name}" (known: ${Object.keys(CAMPAIGNS).join("|")}; or pass --template <path>)`);
  }
  const templatePath = args.template ? resolve(args.template) : known.templatePath;
  if (args.template && !existsSync(templatePath)) throw new Error(`--template not found: ${templatePath}`);
  return {
    name,
    templatePath,
    baseUrl: known.baseUrl,
    expectedBaseUrl: args.template ? null : known.baseUrl,
    api: known.api,
    authHeader: known.authHeader,
  };
}

// Gateway-stall canary rule (README): a probe pair is a dirty window when a
// leg's wall exceeds 8 s or its TTFT exceeds 10 s.
const CANARY_WALL_MS = 8_000;
const CANARY_TTFT_MS = 10_000;

const THINKING_LEVELS = new Set(["off", "minimal", "low", "medium", "high", "xhigh", "max"]);
// `bird`/`app` are artifact/self-eval checks; `quality` is the hidden-test
// campaign. `gen` is a throughput probe with the same threshold rule as probe.
const AB_TASKS = new Set(["bird", "app", "probe", "gen", "quality"]);
const PROBE_LIKE = new Set(["probe", "gen"]);
const P3_TASKS = new Set(["bug", "feature", "both"]);
const PROBE_LEGS = new Set(["both", "pir", "pi"]);

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

function usage() {
  return [
    "Usage: node scripts/perf/ab-ar/run_ab.mjs [flags]",
    "",
    "  --task bird|app|probe|gen|quality",
    "                             ab_bird task (default bird). bird/app are",
    "                             ARTIFACT/self-eval checks, not quality scores.",
    "                             quality -?harder_tasks_campaign.mjs (hidden tests).",
    "  --campaign ar|openai        gateway/template preset (default ar)",
    "  --template <path>          explicit models.json template (overrides the",
    "                             campaign's template path; same key guard)",
    "  --runs N                   repetitions (default 1)",
    "  --pir <exe>                pir binary",
    "  --pi <cmd>                 upstream pi CLI (alias: --pi-cli)",
    "  --model <provider/id>      pinned to the campaign's template model",
    "  --endpoint <url>           baseUrl for runtime models.json copies",
    "  --thinking <level>         default low (off|minimal|low|medium|high|xhigh|max)",
    "  --timeout-ms N             per-run timeout (default 900000)",
    "  --out <dir>                artifacts dir (must be empty unless --force)",
    "  --quality-task <name>      harder_tasks_campaign task (default: all 5)",
    "  --probe-legs both|pir|pi   legs per probe repetition (default both)",
    "  --p3                       also run p3_task_e2e.mjs",
    "  --p3-task bug|feature|both p3 task set (default both)",
    "  --no-canary                skip pre/post gateway-stall canary",
    "  --allow-dirty              continue when the canary is dirty",
    "  --canary-wall-ms N         dirty when probe wall > N (default 8000)",
    "  --canary-ttft-ms N         dirty when probe request-relative TTFT > N (default 10000)",
    "  --force                    replace a non-empty --out dir",
    "  --allow-stale              do not reject a release pir binary older than src/",
    "  --dry-run                  print the plan, start nothing",
    "  --bootstrap-only           write the isolated dirs/launchers, make no model calls",
    "  -h, --help                 this text",
    "",
    `Environment: ${KEY_ENV} (required), PIR_AB_MODEL, PIR_AB_BASE_URL (optional).`,
  ].join("\n");
}

function parseArgs(argv) {
  const args = {
    task: "bird",
    campaign: DEFAULT_CAMPAIGN,
    template: null,
    runs: 1,
    pir: null,
    pi: null,
    model: null,
    endpoint: null,
    thinking: "low",
    timeoutMs: 900_000,
    out: null,
    p3: false,
    p3Task: "both",
    qualityTask: null,
    probeLegs: "both",
    noCanary: false,
    allowDirty: false,
    canaryWallMs: CANARY_WALL_MS,
    canaryTtftMs: CANARY_TTFT_MS,
    force: false,
    allowStale: false,
    // Equalized-surface control: forwarded to both legs of every scored
    // harness (bird/app/quality). Unset = each CLI's own default loadout.
    tools: null,
    dryRun: false,
    bootstrapOnly: false,
    help: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    const next = () => {
      i += 1;
      if (i >= argv.length) throw new Error(`${flag} needs a value`);
      return argv[i];
    };
    switch (flag) {
      case "--task": args.task = next(); break;
      case "--campaign": args.campaign = next(); break;
      case "--template": args.template = next(); break;
      case "--runs": args.runs = Number.parseInt(next(), 10); break;
      case "--pir": args.pir = next(); break;
      case "--pi":
      case "--pi-cli": args.pi = next(); break;
      case "--model": args.model = next(); break;
      case "--endpoint": args.endpoint = next(); break;
      case "--thinking": args.thinking = next(); break;
      case "--tools": args.tools = next(); break;
      case "--timeout-ms": args.timeoutMs = Number.parseInt(next(), 10); break;
      case "--out": args.out = next(); break;
      case "--p3": args.p3 = true; break;
      case "--p3-task": args.p3Task = next(); break;
      case "--quality-task": args.qualityTask = next(); break;
      case "--probe-legs": args.probeLegs = next(); break;
      case "--no-canary": args.noCanary = true; break;
      case "--allow-dirty": args.allowDirty = true; break;
      case "--canary-wall-ms": args.canaryWallMs = Number.parseInt(next(), 10); break;
      case "--canary-ttft-ms": args.canaryTtftMs = Number.parseInt(next(), 10); break;
      case "--force": args.force = true; break;
      case "--allow-stale": args.allowStale = true; break;
      case "--dry-run": args.dryRun = true; break;
      case "--bootstrap-only": args.bootstrapOnly = true; break;
      case "-h":
      case "--help": args.help = true; break;
      default: throw new Error(`unknown flag ${flag} (see --help)`);
    }
  }
  if (!AB_TASKS.has(args.task)) throw new Error(`--task must be one of ${[...AB_TASKS].join("|")}, got ${args.task}`);
  if (!P3_TASKS.has(args.p3Task)) throw new Error(`--p3-task must be one of ${[...P3_TASKS].join("|")}, got ${args.p3Task}`);
  if (!PROBE_LEGS.has(args.probeLegs)) throw new Error(`--probe-legs must be one of ${[...PROBE_LEGS].join("|")}, got ${args.probeLegs}`);
  if (!Number.isInteger(args.runs) || args.runs < 1) throw new Error(`--runs must be a positive integer, got ${args.runs}`);
  if (!Number.isInteger(args.timeoutMs) || args.timeoutMs < 1_000) throw new Error(`--timeout-ms must be >= 1000, got ${args.timeoutMs}`);
  if (!Number.isInteger(args.canaryWallMs) || args.canaryWallMs < 1) throw new Error(`--canary-wall-ms must be a positive integer, got ${args.canaryWallMs}`);
  if (!Number.isInteger(args.canaryTtftMs) || args.canaryTtftMs < 1) throw new Error(`--canary-ttft-ms must be a positive integer, got ${args.canaryTtftMs}`);
  if (!THINKING_LEVELS.has(args.thinking)) throw new Error(`--thinking must be one of ${[...THINKING_LEVELS].join("|")}, got ${args.thinking}`);
  return args;
}

// ---------------------------------------------------------------------------
// Model / endpoint / template contract
// ---------------------------------------------------------------------------

function normalizeModel(value, template) {
  const trimmed = String(value).trim();
  const parts = trimmed.split("/");
  const model = parts.length === 1 ? `${template.provider}/${parts[0]}` : trimmed;
  if (model !== template.model) {
    throw new Error(
      `this A/B lane is pinned to ${template.model} (the "${template.name}" template has only the ` +
      `"${template.provider}" provider and the "${template.modelId}" model); got "${value}"`,
    );
  }
  return model;
}

function parseEndpoint(value) {
  const endpoint = String(value).trim().replace(/\/+$/, "");
  if (!/^https?:\/\/[^\s]+$/i.test(endpoint)) {
    throw new Error(`endpoint must be an http(s) URL, got "${value}"`);
  }
  return endpoint;
}

/**
 * Read the selected committed template and enforce that it contains exactly
 * one provider/model and an env-templated key. This is the guard that keeps a
 * literal credential from ever being committed in this directory.
 *
 * `campaign` carries the expected shape for the selected campaign (`api`,
 * `authHeader`). Everything else -?provider id, model id, baseUrl -?is read
 * from the template, so `--template <path>` works without a registry entry as
 * long as the file matches the expected contract.
 */
function loadTemplate(templatePath, campaign) {
  const raw = readFileSync(templatePath, "utf8");
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (error) {
    throw new Error(`template is not valid JSON (${templatePath}): ${error.message}`);
  }
  const providers = parsed?.providers;
  if (!providers || typeof providers !== "object" || Array.isArray(providers)) {
    throw new Error(`template must have a "providers" object: ${templatePath}`);
  }
  const names = Object.keys(providers);
  if (names.length !== 1) {
    throw new Error(`template must contain exactly ONE provider; found: ${names.join(", ") || "(none)"}`);
  }
  const providerId = names[0];
  const provider = providers[providerId];
  if (campaign.expectedBaseUrl !== null && provider.baseUrl !== campaign.expectedBaseUrl) {
    throw new Error(`template baseUrl must be ${campaign.expectedBaseUrl}; found ${provider.baseUrl}`);
  }
  if (!/^https?:\/\/[^\s]+$/i.test(String(provider.baseUrl ?? ""))) {
    throw new Error(`template baseUrl must be an http(s) URL; found ${JSON.stringify(provider.baseUrl)}`);
  }
  if (provider.api !== campaign.api) {
    throw new Error(`template provider api must be ${campaign.api}; found ${provider.api}`);
  }
  const authHeader = provider.authHeader === true;
  if (authHeader !== campaign.authHeader) {
    throw new Error(
      `template provider authHeader must be ${campaign.authHeader ? "true" : "absent/false"}; ` +
      `found ${JSON.stringify(provider.authHeader)}`,
    );
  }
  const key = String(provider.apiKey ?? "");
  if (key !== `$${KEY_ENV}` && key !== `\${${KEY_ENV}}`) {
    throw new Error(
      `template apiKey must be the $\{${KEY_ENV}} or $${KEY_ENV} template, never a literal key`,
    );
  }
  const models = provider.models;
  if (!Array.isArray(models) || models.length !== 1 || typeof models[0]?.id !== "string") {
    throw new Error(`template must contain exactly one model with a string id`);
  }
  return {
    parsed,
    sha256: createHash("sha256").update(raw).digest("hex"),
    name: campaign.name,
    templatePath,
    provider: providerId,
    providerName: provider.name ?? providerId,
    modelId: models[0].id,
    model: `${providerId}/${models[0].id}`,
    api: provider.api,
    authHeader,
    // The template's own baseUrl; --endpoint / PIR_AB_BASE_URL overrides the
    // runtime copies only (bootstrap rewrites them, the template is untouched).
    endpoint: provider.baseUrl,
  };
}

// ---------------------------------------------------------------------------
// Bootstrap: isolated agent dirs + per-leg launchers
// ---------------------------------------------------------------------------

function defaultPir() {
  const names = IS_WINDOWS ? ["pir.exe"] : ["pir"];
  const profiles = [
    ["target-alt", "release"],
    ["target", "release"],
    ["target-alt", "debug"],
    ["target", "debug"],
  ];
  for (const profile of profiles) {
    for (const name of names) {
      const candidate = join(REPO, ...profile, name);
      if (existsSync(candidate)) return candidate;
    }
  }
  return join(REPO, "target-alt", "release", names[0]);
}

function defaultPi() {
  return IS_WINDOWS ? join(process.env.APPDATA ?? "", "npm", "pi.cmd") : "pi";
}

function looksLikePath(value) {
  return value.includes("/")
    || value.includes("\\")
    || /^[A-Za-z]:/.test(value)
    || /\.(cmd|bat|exe)$/i.test(value);
}

function resolveExecutable(label, value, requireExists = true) {
  if (looksLikePath(value)) {
    if (requireExists && !existsSync(value)) throw new Error(`${label} not found: ${value}`);
    return resolve(value);
  }
  return value; // bare command: resolved through PATH
}

function hashFileSha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function runBinaryVersion(path) {
  const isCmdShim = IS_WINDOWS && /\.(cmd|bat)$/i.test(path);
  const program = isCmdShim ? process.env.ComSpec || "cmd.exe" : path;
  const argv = isCmdShim ? ["/d", "/c", path, "--version"] : ["--version"];
  const result = spawnSync(program, argv, { encoding: "utf8", timeout: 15_000, windowsHide: true });
  // npm .cmd shims on Windows write --version to stderr; accept both streams.
  const stdout = `${result.stdout ?? ""}${result.stderr ?? ""}`.trim();
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
  if (!IS_WINDOWS || !/\.(cmd|bat)$/i.test(path)) return null;
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
  return IS_WINDOWS ? a.toLowerCase() === b.toLowerCase() : a === b;
}

function assertDistinctBinaries(pirInfo, piInfo) {
  if (pirInfo.sha256 && piInfo.sha256 && pirInfo.sha256 === piInfo.sha256) {
    throw new Error(`pir and pi resolve to identical binaries (sha256 ${pirInfo.sha256}); refusing a fake A/B`);
  }
  if (pirInfo.realpath && piInfo.realpath && samePath(pirInfo.realpath, piInfo.realpath)) {
    throw new Error(`pir and pi resolve to the same path: ${pirInfo.realpath}`);
  }
}

function newestMtimeMs(target) {
  let newest = 0;
  const visit = (path) => {
    let stat;
    try {
      stat = statSync(path);
    } catch {
      return;
    }
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

/**
 * A release binary older than the tree it claims to represent produces a
 * meaningless A/B. Debug/profile binaries are not checked: only a path with a
 * `release` segment is gated.
 */
function checkReleaseFreshness(pirInfo, allowStale) {
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
  const record = {
    checked: true,
    stale,
    allowStale: Boolean(allowStale),
    binaryMtime: pirInfo.mtime ?? null,
    newestSourceMtime: newestSource > 0 ? new Date(newestSource).toISOString() : null,
  };
  if (stale && !allowStale) {
    throw new Error(
      `pir release binary is older than src/Cargo.toml/Cargo.lock: ${pirInfo.realpath} (${record.binaryMtime} < ${record.newestSourceMtime}). ` +
      `Rebuild release or pass --allow-stale for an explicitly frozen binary.`,
    );
  }
  return record;
}

function prepareOutDir(outDir, force) {
  const resolved = resolve(outDir);
  const repo = resolve(REPO);
  const key = (value) => (IS_WINDOWS ? value.toLowerCase() : value);
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
    if (entries.length > 0) {
      if (!force) throw new Error(`--out is not empty: ${resolved} (pass --force to overwrite)`);
      rmSync(resolved, { recursive: true, force: true });
    }
  }
  mkdirSync(resolved, { recursive: true });
}

function normalizeChildExit(code) {
  if (code === 3) return 3; // model/artifact verdict FAIL
  if (code === 2) return 1; // harness error
  if (code === 1 || code === null || code === undefined) return 1;
  return code;
}

/**
 * Tag every scored run with the same stall rule as the probe canary. For long
 * tasks a wall > 8 s is normal, so only TTFT/timeout make a run dirty there.
 * Handles both report shapes: ab_bird (`{runs:[...]}`) and the harder campaign
 * (a bare array of rows).
 */
function annotateReportDirty(outDir, task, wallLimit, ttftLimit) {
  const reportPath = join(outDir, "report.json");
  const report = readJson(reportPath);
  if (!report) return [];
  const isQuality = Array.isArray(report);
  const rows = isQuality ? report : (Array.isArray(report.runs) ? report.runs : []);
  if (rows.length === 0) return [];
  const flags = [];
  for (const run of rows) {
    const metrics = isQuality ? run : (run.metrics ?? {});
    const reasons = [];
    if (metrics.timedOut) reasons.push("timedOut");
    const ttft = Number.isFinite(metrics.ttftFromRequestMs)
      ? metrics.ttftFromRequestMs
      : metrics.ttftMs;
    if (!Number.isFinite(ttft)) reasons.push("ttft-missing");
    else if (ttft > ttftLimit) reasons.push(`ttft>${ttftLimit}ms`);
    if (task === "probe" || task === "gen") {
      if (!Number.isFinite(metrics.wallMs)) reasons.push("wall-missing");
      else if (metrics.wallMs > wallLimit) reasons.push(`wall>${wallLimit}ms`);
    }
    const flag = {
      label: isQuality ? (metrics.label ?? "?") : (metrics.label ?? "?"),
      run: isQuality ? (metrics.rep ?? "?") : (metrics.run ?? "?"),
      dirty: reasons.length > 0,
      reasons,
    };
    run.dirty = flag;
    flags.push(flag);
  }
  if (!isQuality) report.dirtyThresholds = { task, wallMs: wallLimit, ttftMs: ttftLimit };
  writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  return flags;
}

function median(values) {
  const nums = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
  if (nums.length === 0) return null;
  const mid = Math.floor(nums.length / 2);
  return nums.length % 2 === 1 ? nums[mid] : Math.round((nums[mid - 1] + nums[mid]) / 2);
}

function pick(obj, path) {
  let cur = obj;
  for (const key of path.split(".")) {
    if (cur === null || cur === undefined) return null;
    cur = cur[key];
  }
  return Number.isFinite(cur) ? cur : null;
}

/**
 * Machine-readable ranking summary (`summary.json`, next to report.md).
 * Medians only, per leg, over STALL-FREE runs. A leg is rankable only when it
 * has kept runs and every kept run passed its verdict; `comparable` requires
 * both legs rankable.
 */
function buildSummary({ outDir, task, meta }) {
  const report = readJson(join(outDir, "report.json"));
  const isQuality = Array.isArray(report);
  const rows = isQuality ? report : (Array.isArray(report?.runs) ? report.runs : []);
  const labels = [...new Set(rows.map((run) => (isQuality ? run.label : run?.metrics?.label)).filter(Boolean))];
  const legs = {};
  for (const label of labels) {
    const all = rows.filter((run) => (isQuality ? run.label : run?.metrics?.label) === label);
    const kept = all.filter((run) => !(run.dirty?.dirty) && !run.timedOut);
    const metric = (path) => median(kept.map((run) => (isQuality ? pick(run, path) : pick(run.metrics, path))));
    // Rankable = it has stall-free data AND every kept run passed. A dropped
    // run (timeout, unscored, TTFT over the limit) does not disqualify the leg;
    // it is reported under droppedReasons and excluded from the medians.
    const verdictOk = kept.length > 0 && kept.every((run) => (isQuality
      ? Number.isFinite(run.score)
      : run?.verdict?.ok === true));
    legs[label] = {
      runs: all.length,
      kept: kept.length,
      dropped: all.length - kept.length,
      droppedReasons: all.filter((run) => run.dirty?.dirty).map((run) => ({
        run: isQuality ? run.rep : run?.metrics?.run,
        reasons: run.dirty?.reasons ?? [],
      })),
      verdict: isQuality
        ? (() => {
          const scored = all.filter((run) => Number.isFinite(run.score));
          const scoreSum = scored.reduce((sum, run) => sum + run.score, 0);
          return {
            scored: scored.length,
            unscored: all.length - scored.length,
            passedChecks: all.reduce((sum, run) => sum + (Number.isFinite(run.passed) ? run.passed : 0), 0),
            totalChecks: all.reduce((sum, run) => sum + (Number.isFinite(run.total) ? run.total : 0), 0),
            scoreSum,
            // Per-scored-run mean only; an unscored run is never a zero score.
            scoreMean: scored.length ? Number((scoreSum / scored.length).toFixed(4)) : null,
          };
        })()
        : { pass: all.filter((run) => run?.verdict?.ok === true).length, fail: all.filter((run) => run?.verdict?.ok !== true).length },
      median: {
        wallMs: metric("wallMs"),
        startupMs: metric("startupMs"),
        ttftMs: metric("ttftMs"),
        ttftFromRequestMs: metric("ttftFromRequestMs"),
        peakRssMb: metric("peakRssMb"),
        turns: metric("turns"),
        totalTokens: isQuality ? metric("tokens") : metric("usage.totalTokens"),
      },
      rankable: kept.length > 0 && verdictOk,
    };
  }
  const labelsPresent = Object.keys(legs);
  const summary = {
    generatedAt: new Date().toISOString(),
    task,
    qualityScore: task === "quality",
    qualityHarnesses: task === "quality"
      ? ["scripts/perf/harder_tasks_campaign.mjs"]
      : ["scripts/perf/harder_tasks_campaign.mjs", "scripts/perf/p3_task_e2e.mjs"],
    model: meta.model,
    endpoint: meta.endpoint,
    thinking: meta.thinking,
    outDir: outDir.replace(/\\/g, "/"),
    binaries: meta.binaries,
    freshness: meta.freshness,
    canaryThresholds: meta.canaryThresholds,
    canary: meta.canary,
    dirty: meta.dirty === true,
    perRunDirty: meta.perRunDirty ?? [],
    rankingRule: [
      "1. verdict first: only verdict-equal, stall-free legs are comparable",
      "2. per-request medians (never means) for wall/startup/TTFT/TTFT(req), kept runs only",
      "3. dropped runs (timeout / TTFT over the canary limit) are listed, never averaged in",
      "4. process-tree peak RSS is an upper bound, sampled every 3 s",
      "5. bird/app are artifact checks; only task=quality is a hidden-test quality score",
    ],
    legs,
    comparable: labelsPresent.length === 2 && labelsPresent.every((label) => legs[label].rankable),
  };
  writeFileSync(join(outDir, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`);
  return summary;
}

function printSummary(summary) {
  if (!summary || Object.keys(summary.legs).length === 0) return;
  console.log("\n[ab-ar] summary (medians over stall-free runs, never means):");
  for (const [label, leg] of Object.entries(summary.legs)) {
    const dropped = leg.droppedReasons.length
      ? ` (dropped: ${leg.droppedReasons.map((d) => `r${d.run}:${d.reasons.join("+")}`).join(", ")})`
      : "";
    const verdict = summary.qualityScore
      ? `score ${leg.verdict.scoreMean ?? "-"} (${leg.verdict.passedChecks}/${leg.verdict.totalChecks} checks)`
      : `verdict ${leg.verdict.pass}/${leg.verdict.pass + leg.verdict.fail} PASS`;
    console.log(
      `[ab-ar]   ${label}: kept ${leg.kept}/${leg.runs}${dropped} | ${verdict} | ` +
      `wall ${leg.median.wallMs ?? "-"}ms | startup ${leg.median.startupMs ?? "-"}ms | ` +
      `TTFT ${leg.median.ttftMs ?? "-"}ms | TTFT(req) ${leg.median.ttftFromRequestMs ?? "-"}ms | ` +
      `RSS ${leg.median.peakRssMb ?? "-"}MB | rankable=${leg.rankable}`,
    );
  }
  console.log(`[ab-ar] comparable A/B: ${summary.comparable ? "yes (both legs rankable)" : "NO (see kept/dropped and verdict)"}`);
  console.log(`[ab-ar] summary json: ${summary.outDir}/summary.json`);
}

function writeWrapper(binDir, name, agentDir, target) {
  if (IS_WINDOWS) {
    const file = join(binDir, `${name}.cmd`);
    // `%*` forwards the original argv tail verbatim; the prompts used by
    // ab_bird/p3 contain no cmd metacharacters (`%`, `&`, `|`, `>`), so the
    // extra cmd layer is safe.
    const call = /\.(cmd|bat)$/i.test(target) ? "call " : "";
    const lines = [
      "@echo off",
      "setlocal",
      `set "PIR_AGENT_DIR=${agentDir}"`,
      `set "PI_CODING_AGENT_DIR=${agentDir}"`,
      `${call}"${target}" %*`,
      "exit /b %ERRORLEVEL%",
      "",
    ];
    writeFileSync(file, lines.join("\r\n"));
    return file;
  }
  const file = join(binDir, `${name}.sh`);
  const quote = (value) => `'${value.replace(/'/g, `'\\''`)}'`;
  const lines = [
    "#!/bin/sh",
    `export PIR_AGENT_DIR=${quote(agentDir)}`,
    `export PI_CODING_AGENT_DIR=${quote(agentDir)}`,
    `exec ${quote(target)} "$@"`,
    "",
  ];
  writeFileSync(file, lines.join("\n"), { mode: 0o755 });
  chmodSync(file, 0o755);
  return file;
}

function bootstrap({ outDir, template, endpoint, thinking, pir, pi }) {
  const home = join(outDir, "home");
  const agentPir = join(outDir, "agent-pir");
  const agentPi = join(outDir, "agent-pi");
  const bin = join(outDir, "bin");
  for (const dir of [home, agentPir, agentPi, bin]) mkdirSync(dir, { recursive: true });
  mkdirSync(join(home, ".pi", "agent"), { recursive: true });

  const runtime = JSON.parse(JSON.stringify(template.parsed));
  runtime.providers[template.provider].baseUrl = endpoint;
  const modelsJson = `${JSON.stringify(runtime, null, 2)}\n`;
  const settingsJson = `${JSON.stringify(
    {
      defaultProvider: template.provider,
      defaultModel: template.modelId,
      defaultThinkingLevel: thinking,
    },
    null,
    2,
  )}\n`;

  // home/.pi/agent is what ab_bird/p3 copy from (they derive the source dir
  // from HOME/USERPROFILE); agent-pir/agent-pi are the dirs the launchers pin.
  for (const dir of [join(home, ".pi", "agent"), agentPir, agentPi]) {
    writeFileSync(join(dir, "models.json"), modelsJson);
    writeFileSync(join(dir, "settings.json"), settingsJson);
  }

  return {
    home,
    agentPir,
    agentPi,
    bin,
    pirWrapper: writeWrapper(bin, "pir-leg", agentPir, pir),
    piWrapper: writeWrapper(bin, "pi-leg", agentPi, pi),
  };
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

function displayCommand(script, argv) {
  const quote = (arg) => (/[\s"]/.test(arg) ? JSON.stringify(arg) : arg);
  return [process.execPath, script, ...argv].map(quote).join(" ");
}

function spawnNode(script, argv, env) {
  return new Promise((settle) => {
    const child = spawn(process.execPath, [script, ...argv], {
      env,
      stdio: "inherit",
      windowsHide: false,
    });
    child.on("error", (error) => settle({ code: 1, error: String(error?.message ?? error) }));
    child.on("close", (code, signal) => settle({ code: code ?? (signal ? 1 : 0), signal: signal ?? null }));
  });
}

function abBirdArgs({ task, runs, out, pirWrapper, piWrapper, model, thinking, timeoutMs, force = false, probeLegs = "both", allowStale = false, tools = null }) {
  return [
    "--pir", pirWrapper,
    "--pi-cli", piWrapper,
    "--model", model,
    "--task", task,
    "--runs", String(runs),
    "--thinking", thinking,
    "--timeout-ms", String(timeoutMs),
    "--out", out,
    // Equalized tool surface (both legs); omitted = each CLI's default.
    ...(tools ? ["--tools", tools] : []),
    // run_ab has already validated the release binary's freshness; ab_bird
    // would apply the same rule again against the same tree, so the two can
    // never disagree -?but a frozen binary allowed here must also be allowed
    // there (--allow-stale is forwarded by the caller when it was passed).
    ...(allowStale ? ["--allow-stale"] : []),
    // Fair prefixes: pi also reads ~/.agents/skills, which the isolated agent
    // dir alone does not hide; both legs must run without skills.
    "--no-skills",
    // A probe may run ONE leg per repetition (--probe-legs); the verdict is
    // then per leg, not "both legs must appear in every repetition".
    ...(PROBE_LIKE.has(task) && probeLegs !== "both" ? ["--legs", probeLegs] : []),
    // run_ab has already prepared/validated outDir and writes run-meta.json
    // there before spawning ab_bird; ab_bird must allow that non-empty dir.
    "--force",
  ];
}

/**
 * harder_tasks_campaign.mjs arguments. It has no launcher-identity env hook:
 * it runs the real binaries directly with the isolated HOME that run_ab
 * exports and creates its own fresh agent dir per leg (stricter than the
 * shared launcher dirs), so the binaries come from PIR_BIN/PI_CLI in env.
 */
function qualityArgs({ runs, out, model, timeoutMs, task, thinking, allowStale = false, force = false, tools = null }) {
  return [
    "--runs", String(runs),
    "--out", out,
    "--model", model,
    "--thinking", thinking,
    "--timeout-ms", String(timeoutMs),
    ...(task ? ["--task", task] : []),
    ...(tools ? ["--tools", tools] : []),
    ...(allowStale ? ["--allow-stale"] : []),
    ...(force ? ["--force"] : []),
  ];
}

// ---------------------------------------------------------------------------
// Canary: probe wall / TTFT thresholds
// ---------------------------------------------------------------------------

function readJson(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return null;
  }
}

/**
 * Evaluate a probe report.
 *
 * A repetition does NOT have to contain both legs: with `--probe-legs pir` a
 * window has only pir legs, and that is a legitimate canary. The rule is
 * therefore "every leg present must be clean", plus "at least one leg per
 * repetition was measured". A dirty/failed leg is a problem (reported with the
 * leg name), never a hard requirement that the other leg also ran.
 */
function evaluateProbeReport(dir, wallLimit = CANARY_WALL_MS, ttftLimit = CANARY_TTFT_MS, expectLegs = "both") {
  const report = readJson(join(dir, "report.json"));
  if (!report || !Array.isArray(report.runs) || report.runs.length === 0) {
    return {
      legs: [], thresholdProblems: [`no readable report.json in ${dir}`], verdictFailures: [],
      problems: [`no readable report.json in ${dir}`], dirty: true, failed: false, exitCode: null,
    };
  }
  const legs = report.runs.map((run) => ({
    label: run?.metrics?.label ?? "?",
    run: run?.metrics?.run ?? "?",
    wallMs: Number.isFinite(run?.metrics?.wallMs) ? run.metrics.wallMs : null,
    ttftMs: Number.isFinite(run?.metrics?.ttftMs) ? run.metrics.ttftMs : null,
    requestAtMs: Number.isFinite(run?.metrics?.requestAtMs) ? run.metrics.requestAtMs : null,
    ttftFromRequestMs: Number.isFinite(run?.metrics?.ttftFromRequestMs) ? run.metrics.ttftFromRequestMs : null,
    ok: run?.verdict?.ok === true,
    notes: Array.isArray(run?.verdict?.notes) ? run.verdict.notes : [],
  }));
  // Two failure classes, so the exit code stays meaningful:
  //  - threshold problems = the WINDOW is dirty (wall/TTFT/no measurement) -?2
  //  - verdict failures   = the LEG/probe itself failed -?3
  const thresholdProblems = [];
  const verdictFailures = [];
  const reps = new Map();
  for (const leg of legs) {
    const who = `${leg.label} r${leg.run}`;
    reps.set(leg.run, (reps.get(leg.run) ?? 0) + 1);
    if (leg.wallMs === null) thresholdProblems.push(`${who}: wall missing`);
    else if (leg.wallMs > wallLimit) thresholdProblems.push(`${who}: wall ${leg.wallMs} ms > ${wallLimit} ms`);
    if (leg.ttftMs === null && leg.ttftFromRequestMs === null) {
      thresholdProblems.push(`${who}: TTFT missing (no delta observed)`);
    } else {
      const ttft = leg.ttftFromRequestMs ?? leg.ttftMs;
      if (ttft > ttftLimit) thresholdProblems.push(`${who}: TTFT(req) ${ttft} ms > ${ttftLimit} ms`);
    }
    if (!leg.ok) {
      const timedOut = leg.notes.some((note) => /timed out/i.test(note));
      const sink = timedOut ? thresholdProblems : verdictFailures;
      sink.push(`${who}: probe verdict FAIL${leg.notes.length ? ` (${leg.notes.join("; ")})` : ""}`);
    }
  }
  // At least one leg per repetition. (Both legs are preferred, but a
  // one-leg-per-repetition probe is a valid design and must not read as dirty.)
  if (reps.size === 0) thresholdProblems.push("no probe legs recorded");
  const wantLegs = expectLegs === "both" ? null : new Set([expectLegs]);
  if (wantLegs) {
    const wrong = legs.filter((leg) => !wantLegs.has(leg.label));
    for (const leg of wrong) thresholdProblems.push(`${leg.label} r${leg.run}: unexpected leg (--probe-legs ${expectLegs})`);
  }
  const problems = [...thresholdProblems, ...verdictFailures];
  return {
    legs,
    thresholdProblems,
    verdictFailures,
    problems,
    dirty: thresholdProblems.length > 0,
    failed: verdictFailures.length > 0,
    exitCode: report.exitCode ?? null,
  };
}

/**
 * Compare the binary identity recorded INSIDE a campaign report with the
 * binaries this runner hashed before the run. A mismatch means the experiment
 * changed underneath us (rebuild, swapped launcher); never score it.
 */
function campaignBinaryDrift(reportDir, binaries) {
  const report = readJson(join(reportDir, "report.json"));
  if (!report || !binaries) return [];
  const seenBinaries = Array.isArray(report) ? null : report.binaries;
  if (!seenBinaries) return [];
  const drift = [];
  for (const leg of ["pir", "pi"]) {
    const expected = binaries[leg]?.sha256 ?? null;
    const seen = seenBinaries[leg]?.sha256 ?? null;
    if (expected && seen && expected !== seen) {
      drift.push(`${leg}: runner ${expected.slice(0, 12)} != report ${seen.slice(0, 12)}`);
    }
  }
  return drift;
}

function printProbeLegs(prefix, evaluation) {
  for (const leg of evaluation.legs) {
    console.log(
      `[ab-ar] ${prefix} ${leg.label} r${leg.run}: wall=${leg.wallMs ?? "-"}ms ` +
      `ttft=${leg.ttftMs ?? "-"}ms ttft_req=${leg.ttftFromRequestMs ?? "-"}ms ` +
      `verdict=${leg.ok ? "PASS" : "FAIL"}`,
    );
  }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

function timestamp() {
  return new Date().toISOString()
    .replace(/[-:]/g, "")
    .replace("T", "-")
    .replace(/\.(\d{3})Z$/, "$1");
}

function printArtifacts(outDir, extra = {}) {
  const dir = resolve(outDir).replace(/\\/g, "/");
  console.log("");
  console.log(`[ab-ar] raw artifacts dir: ${dir}`);
  console.log(`[ab-ar]   reports:       ${dir}/report.md, ${dir}/report.json`);
  console.log(`[ab-ar]   per-run work:  ${dir}/work/<task>-<pir|pi>-r<N>/`);
  console.log(`[ab-ar]   streams:       ${dir}/events-*.jsonl, ${dir}/stderr-*.log`);
  console.log(`[ab-ar]   agent dirs:    ${dir}/agent-pir, ${dir}/agent-pi (models template, no key)`);
  console.log(`[ab-ar]   run meta:      ${dir}/run-meta.json`);
  if (extra.canaryDirs?.length) console.log(`[ab-ar]   canary:        ${extra.canaryDirs.join(", ")}`);
  if (extra.quality) console.log(`[ab-ar]   quality:       ${extra.quality} (report.md, report.json, summary.json)`);
  if (extra.p3) console.log(`[ab-ar]   p3:            ${extra.p3} (report.md)`);
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    console.log(usage());
    return 0;
  }

  const campaign = resolveCampaign(args);
  const template = loadTemplate(campaign.templatePath, campaign);
  const model = normalizeModel(args.model ?? process.env.PIR_AB_MODEL ?? template.model, template);
  const endpoint = parseEndpoint(args.endpoint ?? process.env.PIR_AB_BASE_URL ?? template.endpoint);
  // Dry-run/bootstrap must work on a machine that has not installed pi/pir:
  // the plan is useful before the binaries exist. A real run still requires
  // both paths and fails loudly below.
  const requireBinaries = !args.dryRun && !args.bootstrapOnly;
  const pir = args.pir
    ? resolveExecutable("pir", args.pir, requireBinaries)
    : resolveExecutable("pir", defaultPir(), requireBinaries);
  const pi = args.pi
    ? resolveExecutable("pi", args.pi, requireBinaries)
    : resolveExecutable("pi", defaultPi(), requireBinaries);
  const apiKey = process.env[KEY_ENV] ?? "";
  const keyPresent = apiKey.trim().length > 0 && !/\r|\n/.test(apiKey);
  const outDir = resolve(args.out ?? join(tmpdir(), "pir-ab-ar", `${timestamp()}-${args.task}`));

  if (args.dryRun) {
    console.log("[ab-ar] dry run -?nothing is started, no binary is resolved or hashed");
    console.log(`[ab-ar] campaign:   ${template.name} (api ${template.api}${template.authHeader ? ", authHeader Bearer" : ""})`);
    console.log(`[ab-ar] template:   ${template.templatePath} (sha256 ${template.sha256})`);
    console.log(`[ab-ar] model:      ${model}`);
    console.log(`[ab-ar] endpoint:   ${endpoint}`);
    console.log(`[ab-ar] thinking:   ${args.thinking}`);
    console.log(`[ab-ar] task:       ${args.task}${args.task === "quality" ? " (hidden-test quality scores)" : args.task === "bird" || args.task === "app" ? " (ARTIFACT/self-eval check, NOT a quality score)" : ""}`);
    console.log(`[ab-ar] pir:        ${pir}${requireBinaries ? "" : "  (existence checked only on a real run)"}`);
    console.log(`[ab-ar] pi:         ${pi}${requireBinaries ? "" : "  (existence checked only on a real run)"}`);
    console.log(`[ab-ar] ${KEY_ENV}: ${keyPresent ? "PRESENT (value never read/logged)" : "MISSING (required for a real run)"}`);
    console.log(`[ab-ar] canary:     wall>${args.canaryWallMs}ms or TTFT(req)>${args.canaryTtftMs}ms -?dirty; legs=${args.probeLegs}`);
    console.log(`[ab-ar] out:        ${outDir}`);
    console.log(`[ab-ar] bootstrap:  ${outDir}/agent-pir, ${outDir}/agent-pi, ${outDir}/bin/*-leg.${IS_WINDOWS ? "cmd" : "sh"}`);
    if (args.task === "quality") {
      const qArgs = qualityArgs({
        runs: args.runs,
        out: join(outDir, "quality"),
        model,
        timeoutMs: args.timeoutMs,
        task: args.qualityTask,
        thinking: args.thinking,
        allowStale: args.allowStale,
        tools: args.tools,
        force: args.force,
      });
      console.log(`[ab-ar] command:    PIR_BIN=${pir} PI_CLI=${pi} ${displayCommand(QUALITY_PATH, qArgs)}`);
    } else {
      const mainArgs = abBirdArgs({
        task: args.task,
        runs: args.runs,
        out: outDir,
        pirWrapper: join(outDir, "bin", IS_WINDOWS ? "pir-leg.cmd" : "pir-leg.sh"),
        piWrapper: join(outDir, "bin", IS_WINDOWS ? "pi-leg.cmd" : "pi-leg.sh"),
        model,
        thinking: args.thinking,
        timeoutMs: args.timeoutMs,
        force: args.force,
        probeLegs: args.probeLegs,
        allowStale: args.allowStale,
        tools: args.tools,
      });
      console.log(`[ab-ar] command:    ${displayCommand(AB_BIRD_PATH, mainArgs)}`);
    }
    if (!PROBE_LIKE.has(args.task) && !args.noCanary) {
      const canaryArgs = abBirdArgs({
        task: "probe",
        runs: 1,
        out: join(outDir, "canary-before"),
        pirWrapper: join(outDir, "bin", IS_WINDOWS ? "pir-leg.cmd" : "pir-leg.sh"),
        piWrapper: join(outDir, "bin", IS_WINDOWS ? "pi-leg.cmd" : "pi-leg.sh"),
        model,
        thinking: args.thinking,
        timeoutMs: Math.min(args.timeoutMs, 120_000),
        force: args.force,
        probeLegs: args.probeLegs,
        allowStale: args.allowStale,
        tools: args.tools,
      });
      console.log(`[ab-ar] canary:     ${displayCommand(AB_BIRD_PATH, canaryArgs)}`);
    }
    if (args.p3) {
      const p3Args = [
        "--cli", join(outDir, "bin", IS_WINDOWS ? "pir-leg.cmd" : "pir-leg.sh"),
        "--pi-cli", join(outDir, "bin", IS_WINDOWS ? "pi-leg.cmd" : "pi-leg.sh"),
        "--model", model, "--task", args.p3Task, "--runs", String(args.runs),
        "--timeout-ms", String(args.timeoutMs), "--out", join(outDir, "p3"),
        ...(args.allowStale ? ["--allow-stale"] : []),
        ...(args.force ? ["--force"] : []),
      ];
      console.log(`[ab-ar] p3:         ${displayCommand(P3_PATH, p3Args)}`);
    }
    return 0;
  }

  if (!keyPresent && !args.bootstrapOnly) {
    throw new Error(
      `${KEY_ENV} is not set (or contains a newline). Export the ar gateway key first; ` +
      `this lane never writes it to disk. Example: export ${KEY_ENV}='<key>'`,
    );
  }
  if (!existsSync(AB_BIRD_PATH)) throw new Error(`ab_bird not found: ${AB_BIRD_PATH}`);
  if (args.p3 && !existsSync(P3_PATH)) throw new Error(`p3 harness not found: ${P3_PATH}`);

  // Record and validate exactly what will run before any model call. A
  // bootstrap-only invocation must still work without the binaries installed.
  let binaries = null;
  let freshness = null;
  if (!args.bootstrapOnly) {
    binaries = {
      pir: binaryInfo("pir", pir),
      pi: binaryInfo("pi", pi),
    };
    assertDistinctBinaries(binaries.pir, binaries.pi);
    if (!binaries.pir.version) throw new Error(`could not read pir --version: ${pir}`);
    if (!binaries.pi.version) throw new Error(`could not read pi --version: ${pi}`);
    freshness = checkReleaseFreshness(binaries.pir, args.allowStale);
  }

  prepareOutDir(outDir, args.force);
  const boot = bootstrap({ outDir, template, endpoint, thinking: args.thinking, pir, pi });
  const childEnv = {
    ...process.env,
    HOME: boot.home,
    USERPROFILE: boot.home,
    [KEY_ENV]: apiKey,
    PIR_AB_MODEL: model,
    PIR_AB_BASE_URL: endpoint,
    // ab_bird hashes the real targets rather than the agent-dir launchers.
    PIR_AB_PIR_BIN: pir,
    PIR_AB_PI_BIN: pi,
  };

  const meta = {
    createdAt: new Date().toISOString(),
    outDir,
    task: args.task,
    runs: args.runs,
    model,
    endpoint,
    thinking: args.thinking,
    timeoutMs: args.timeoutMs,
    pir,
    pi,
    binaries,
    freshness,
    canaryThresholds: { wallMs: args.canaryWallMs, ttftMs: args.canaryTtftMs },
    noSkills: true,
    noSession: true,
    force: args.force,
    allowStale: args.allowStale,
        tools: args.tools,
    campaign: {
      name: template.name,
      api: template.api,
      authHeader: template.authHeader,
    },
    template: {
      path: template.templatePath,
      sha256: template.sha256,
      providers: [template.provider],
      models: [template.modelId],
      apiKeyTemplate: `\${${KEY_ENV}}`,
    },
    env: {
      [KEY_ENV]: keyPresent ? "PRESENT (not logged)" : "missing",
      PIR_AB_MODEL: model,
      PIR_AB_BASE_URL: endpoint,
      HOME: boot.home,
      USERPROFILE: boot.home,
    },
    agentDirs: { pir: boot.agentPir, pi: boot.agentPi },
    launchers: { pir: boot.pirWrapper, pi: boot.piWrapper },
    commands: [],
    canary: {},
    dirty: false,
    allowDirty: args.allowDirty,
    bootstrapOnly: args.bootstrapOnly,
    exitCodes: {},
  };
  const writeMeta = () => writeFileSync(join(outDir, "run-meta.json"), `${JSON.stringify(meta, null, 2)}\n`);
  writeMeta();

  if (args.bootstrapOnly) {
    console.log(`[ab-ar] bootstrap only -?no model calls`);
    console.log(`[ab-ar] template: ${template.templatePath} (sha256 ${template.sha256})`);
    console.log(`[ab-ar] launchers: ${boot.pirWrapper} | ${boot.piWrapper}`);
    meta.exitCodes.overall = 0;
    writeMeta();
    printArtifacts(outDir, {});
    return 0;
  }

  console.log(`[ab-ar] model ${model} via ${endpoint}`);
  console.log(`[ab-ar] agent dirs: ${boot.agentPir} | ${boot.agentPi}`);
  console.log(`[ab-ar] ${KEY_ENV}: PRESENT (never written to disk)`);

  const mainArgs = abBirdArgs({
    task: args.task,
    runs: args.runs,
    out: outDir,
    pirWrapper: boot.pirWrapper,
    piWrapper: boot.piWrapper,
    model,
    thinking: args.thinking,
    timeoutMs: args.timeoutMs,
    force: args.force,
    probeLegs: args.probeLegs,
    allowStale: args.allowStale,
        tools: args.tools,
  });

  let exitCode = 0;
  let canaryDirty = false;
  // `--task quality` routes to the hidden-test campaign instead of ab_bird;
  // bird/app alone are artifact checks and cannot score quality.
  const isQuality = args.task === "quality";
  let qualityResult = null;

  const runCanary = async (phase) => {
    const dir = join(outDir, `canary-${phase}`);
    const argv = abBirdArgs({
      task: "probe",
      runs: 1,
      out: dir,
      pirWrapper: boot.pirWrapper,
      piWrapper: boot.piWrapper,
      model,
      thinking: args.thinking,
      timeoutMs: Math.min(args.timeoutMs, 120_000),
      force: args.force,
      probeLegs: args.probeLegs,
      allowStale: args.allowStale,
        tools: args.tools,
    });
    meta.commands.push(displayCommand(AB_BIRD_PATH, argv));
    console.log(`\n[ab-ar] canary (${phase}): probe pair`);
    const result = await spawnNode(AB_BIRD_PATH, argv, childEnv);
    const evaluation = evaluateProbeReport(dir, args.canaryWallMs, args.canaryTtftMs, args.probeLegs);
    // Record the binary identity the canary itself observed, and refuse a
    // campaign when it differs from the binaries this runner hashed: a binary
    // swapped between canary and campaign is not the same experiment.
    const canaryReport = readJson(join(dir, "report.json"));
    const observed = canaryReport?.binaries
      ? Object.fromEntries(["pir", "pi"].map((leg) => [leg, {
        realpath: canaryReport.binaries[leg]?.realpath ?? null,
        sha256: canaryReport.binaries[leg]?.sha256 ?? null,
        version: canaryReport.binaries[leg]?.version ?? null,
      }]))
      : null;
    const drift = [];
    if (observed && binaries) {
      for (const leg of ["pir", "pi"]) {
        const expected = binaries[leg]?.sha256 ?? null;
        const seen = observed[leg]?.sha256 ?? null;
        if (expected && seen && expected !== seen) drift.push(`${leg}: campaign ${expected.slice(0, 12)} != canary ${seen.slice(0, 12)}`);
      }
    }
    if (drift.length) {
      canaryDirty = true;
      console.warn(`[ab-ar] BINARY DRIFT during canary (${phase}): ${drift.join("; ")}`);
    }
    const record = {
      dir, exitCode: result.code, dirty: evaluation.dirty, problems: evaluation.problems,
      legs: evaluation.legs, binaries: observed, binaryDrift: drift,
    };
    meta.canary[phase] = record;
    printProbeLegs(`canary ${phase}`, evaluation);
    if (evaluation.dirty) {
      canaryDirty = true;
      console.warn(`[ab-ar] DIRTY WINDOW (${phase}):`);
      for (const problem of evaluation.problems) console.warn(`[ab-ar]   - ${problem}`);
      if (!args.allowDirty) {
        console.warn(`[ab-ar] refusing to score this window; rerun later or pass --allow-dirty`);
      } else {
        console.warn(`[ab-ar] --allow-dirty: continuing, campaign will be tagged dirty in run-meta.json`);
      }
    } else if (result.code !== 0) {
      canaryDirty = true;
      console.warn(`[ab-ar] canary exit ${result.code} without a threshold breach; treating as dirty`);
    } else {
      console.log(`[ab-ar] canary (${phase}): clean`);
    }
    writeMeta();
    return record;
  };

  try {
    if (!PROBE_LIKE.has(args.task) && !args.noCanary) {
      await runCanary("before");
      if (canaryDirty && !args.allowDirty) {
        exitCode = 2;
        return exitCode;
      }
    }

    if (isQuality) {
      const qualityOut = join(outDir, "quality");
      const qArgs = qualityArgs({
        runs: args.runs,
        out: qualityOut,
        model,
        timeoutMs: args.timeoutMs,
        task: args.qualityTask,
        thinking: args.thinking,
        allowStale: args.allowStale,
        tools: args.tools,
        force: args.force,
      });
      meta.commands.push(displayCommand(QUALITY_PATH, qArgs));
      // harder_tasks_campaign reads its binaries from PIR_BIN/PI_CLI and gets
      // the isolated HOME from childEnv; it creates a fresh agent dir per leg.
      const qualityEnv = { ...childEnv, PIR_BIN: pir, PI_CLI: pi };
      console.log(`\n[ab-ar] quality campaign: 5 hidden-test tasks - ${args.runs} (pir/pi interleaved per task)`);
      const qResult = await spawnNode(QUALITY_PATH, qArgs, qualityEnv);
      meta.exitCodes.quality = qResult.code;
      meta.exitCodes.qualityRaw = qResult.code;
      if (qResult.error) meta.exitCodes.qualityError = qResult.error;
      if (qResult.code === 3) meta.verdictFailed = true;
      if (qResult.code === 1 || qResult.code === 2) meta.harnessFailed = true;
      exitCode = normalizeChildExit(qResult.code);
      meta.perRunDirty = annotateReportDirty(qualityOut, "quality", args.canaryWallMs, args.canaryTtftMs);
      meta.campaignDirty = meta.perRunDirty.some((run) => run.dirty);
      qualityResult = { dir: qualityOut, exitCode: qResult.code };
    } else {
      console.log(`\n[ab-ar] campaign: ${args.task} - ${args.runs} (pir/pi interleaved per repetition)`);
      if (args.task === "bird" || args.task === "app") {
        console.log(`[ab-ar] note: \`${args.task}\` is an ARTIFACT/self-eval check, not a hidden-test quality score; use --task quality.`);
      }
      meta.commands.push(displayCommand(AB_BIRD_PATH, mainArgs));
      const mainResult = await spawnNode(AB_BIRD_PATH, mainArgs, childEnv);
      meta.exitCodes.abBird = mainResult.code;
      meta.exitCodes.abBirdRaw = mainResult.code;
      if (mainResult.error) meta.exitCodes.abBirdError = mainResult.error;
      if (mainResult.code === 3) meta.verdictFailed = true;
      if (mainResult.code === 1 || mainResult.code === 2) meta.harnessFailed = true;
      exitCode = normalizeChildExit(mainResult.code);
      meta.perRunDirty = annotateReportDirty(outDir, args.task, args.canaryWallMs, args.canaryTtftMs);
      meta.campaignDirty = meta.perRunDirty.some((run) => run.dirty);
      const drift = campaignBinaryDrift(outDir, binaries);
      if (drift.length) {
        meta.binaryDrift = drift;
        meta.campaignDirty = true;
        console.warn(`[ab-ar] BINARY DRIFT between the recorded identity and the campaign report: ${drift.join("; ")}`);
        if (!args.allowDirty) exitCode = 2;
      }
    }

    if (PROBE_LIKE.has(args.task)) {
      const evaluation = evaluateProbeReport(outDir, args.canaryWallMs, args.canaryTtftMs, args.probeLegs);
      meta.canary.probe = {
        dir: outDir,
        dirty: evaluation.dirty,
        verdictFailed: evaluation.failed,
        problems: evaluation.problems,
        legs: evaluation.legs,
      };
      printProbeLegs(args.task, evaluation);
      if (evaluation.dirty) {
        canaryDirty = true;
        console.warn(`[ab-ar] DIRTY WINDOW (${args.task}):`);
        for (const problem of evaluation.thresholdProblems) console.warn(`[ab-ar]   - ${problem}`);
        if (!args.allowDirty) {
          console.warn(`[ab-ar] ${args.task} window is dirty; rerun later or pass --allow-dirty`);
          exitCode = 2; // a dirty window wins over a verdict FAIL
        }
      }
      // A clean window whose probe verdict failed is a real FAIL, not a dirty
      // window: exit 3 so callers can tell "rerun later" from "model failed".
      // A harness error (1) is never relabeled as a model verdict.
      if (evaluation.failed) {
        if (exitCode === 0 || exitCode === 3) exitCode = 3;
        console.warn(`[ab-ar] ${args.task} verdict FAIL:`);
        for (const problem of evaluation.verdictFailures) console.warn(`[ab-ar]   - ${problem}`);
      }
    } else if (!args.noCanary) {
      await runCanary("after");
      if (canaryDirty && !args.allowDirty) exitCode = 2; // dirty wins over 3
    }

    if (args.p3) {
      const p3Out = join(outDir, "p3");
      const p3Args = [
        "--cli", boot.pirWrapper,
        "--pi-cli", boot.piWrapper,
        "--model", model,
        "--task", args.p3Task,
        "--runs", String(args.runs),
        "--timeout-ms", String(args.timeoutMs),
        "--out", p3Out,
        ...(args.allowStale ? ["--allow-stale"] : []),
        ...(args.force ? ["--force"] : []),
      ];
      meta.commands.push(displayCommand(P3_PATH, p3Args));
      console.log(`\n[ab-ar] p3 real-task E2E (${args.p3Task})`);
      const p3Result = await spawnNode(P3_PATH, p3Args, childEnv);
      meta.exitCodes.p3 = p3Result.code;
      meta.exitCodes.p3Raw = p3Result.code;
      if (p3Result.error) meta.exitCodes.p3Error = p3Result.error;
      if (p3Result.code === 3) meta.verdictFailed = true;
      if (p3Result.code === 1 || p3Result.code === 2) {
        meta.harnessFailed = true;
        // A dirty gateway window (2) must survive a p3 harness error.
        if (exitCode !== 2) exitCode = 1;
      } else if (exitCode === 0 && p3Result.code !== 0) {
        exitCode = normalizeChildExit(p3Result.code);
      }
    }

    meta.dirty = canaryDirty;
    meta.exitCodes.overall = exitCode;
    try {
      meta.summary = buildSummary({ outDir: isQuality ? qualityResult.dir : outDir, task: args.task, meta });
    } catch (error) {
      meta.summaryError = String(error?.message ?? error);
    }
    writeMeta();
    return exitCode;
  } finally {
    meta.dirty = canaryDirty;
    meta.exitCodes.overall = exitCode;
    try { writeMeta(); } catch { /* best effort */ }
    if (meta.summary) printSummary(meta.summary);
    printArtifacts(outDir, {
      canaryDirs: Object.keys(meta.canary)
        .filter((key) => key === "before" || key === "after")
        .map((key) => join(outDir, `canary-${key}`).replace(/\\/g, "/")),
      p3: args.p3 ? join(outDir, "p3").replace(/\\/g, "/") : null,
      quality: isQuality && qualityResult ? qualityResult.dir.replace(/\\/g, "/") : null,
    });
  }
}

main()
  .then((code) => {
    process.exitCode = code ?? 0;
  })
  .catch((error) => {
    console.error(`[ab-ar] ERROR: ${error?.message ?? error}`);
    process.exitCode = 1;
  });

