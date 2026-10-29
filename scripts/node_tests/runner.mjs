// runner.mjs — run a vendored subset of the official Node.js lib test suite
// (test/parallel) against the pir extension runtime, Deno-style.
//
// Pipeline per test file:
//   1. read scripts/node_tests/vendored/<file> (raw Node source, unmodified)
//   2. apply the recorded require-rewrites + the recorded common.js patches
//      (see PATCHES / REWRITES below — the pir equivalent of Deno's
//      minimal adaptation layer)
//   3. write the transformed copy to work/<file>.cjs
//   4. `pir js work/<file>.cjs` (real stdio, real exit codes), 20s timeout
//   5. classify the outcome (pass / fail / skip + failure category)
//
// `node:test`-based files are scored by subtest counters (marker line
// `__NODERESULT__ {...}` emitted by shims/test.cjs); plain scripts are
// scored by exit code + mustCall-audit markers on stdout.
//
// Usage:
//   node scripts/node_tests/runner.mjs [--pir <path>] [--node]
//        [--only test-buffer-alloc.js,...] [--out <report.md>] [--quiet]
//        [--check-hashes] [--skip-hash-check]
//   --node  run the same transformed files with the system node as a
//           control pass (validates the harness itself).
//   Every run (including --node) first verifies vendored/** against
//   vendored.sha256 and refuses to stage work/ on any mismatch — vendored
//   edits would be masked by the control pass otherwise.
//   --check-hashes      only run the integrity check, then exit 0/1
//                       (works without a pir binary).
//   --skip-hash-check   bypass the integrity gate. LOCAL DIAGNOSIS ONLY.

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
  rmSync,
  openSync,
  writeSync,
  closeSync,
  statSync,
} from "node:fs";
import { join, dirname, basename } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = join(HERE, "..", "..");
const VENDOR = join(HERE, "vendored");
// Mirror the upstream layout (test/parallel): some tests assert on
// __dirname/__filename, so the transformed copy must live in a directory
// named test/parallel under the work root. common/ and shims/ are COPIED
// next to the transformed files at run time — pir js_mode fences module
// loads to the script's own directory tree, so ../-escapes out of work/
// fail with EACCES.
const WORK = join(HERE, "work", "test", "parallel");
const SHIMS = join(HERE, "shims");

// Source the vendored files come from. Every file in vendored/ is fetched
// verbatim from this tag; see README.md for the re-fetch recipe.
export const NODE_TAG = "v24.19.0";

// ---------------------------------------------------------------------------
// 完整性门 (integrity gate)
//
// vendored/** 必须逐字节等于 vendored.sha256 记录的基线。哈希来源：上游
// nodejs/node <NODE_TAG> 的 test/ 原文件（test/parallel/*.js +
// test/common/{index,fixtures,tmpdir,hijackstdio}.js）。仓库记录的三处
// common 补丁（见 COMMON_PATCHES）是 runner 在 stage work/ 时的改写，不进
// vendored/ —— 被哈希的是未改写的上游原文件。
//
// 为什么必须有这道门：runner 每一轮都把 vendored/ 改写成 work/ 副本，
// --node 控制组跑的是同一份副本。直接改 vendored/test-*.js（例如删掉一条
// 断言）会同时让 pir 侧和"验证 harness 本身"的控制组变绿，机制上无从察觉；
// 此前只靠 README 的"逐字节不改"纪律约束。校验必须在 stage work/ 之前。
//
// 逃生门 --skip-hash-check 仅用于本地诊断；任何对外报告/结论都必须走完整
// 校验（见 README.md「完整性门」）。
// ---------------------------------------------------------------------------
const HASH_FILE = join(HERE, "vendored.sha256");
const SKIP_HASH_CHECK = process.argv.includes("--skip-hash-check");
const CHECK_HASHES_ONLY = process.argv.includes("--check-hashes");

const HASH_HINT =
  `vendored/ 必须逐字节保持上游 node ${NODE_TAG} 原样；改动会被控制组掩盖` +
  `（--node 跑的是同一份 work/ 改写副本，删断言/改期望值会让两边同时变绿）。` +
  `请从上游重取该文件后用 \`node scripts/node_tests/runner.mjs --check-hashes\` 复验；` +
  `逃生门 --skip-hash-check 仅限本地诊断，不得用于任何对外结论。`;

function sha256Of(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

// vendored/** 的全部相对路径（posix 分隔符，排序）。
function listVendoredFiles() {
  const acc = [];
  const walk = (dir, rel) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const r = rel ? `${rel}/${e.name}` : e.name;
      if (e.isDirectory()) walk(join(dir, e.name), r);
      else acc.push(r);
    }
  };
  walk(VENDOR, "vendored");
  return acc.sort();
}

// 解析 vendored.sha256（GNU sha256sum "hex  path" 格式；# 与空行忽略）。
function readHashBaseline() {
  const entries = new Map();
  if (!existsSync(HASH_FILE)) return { entries, error: `哈希基线文件不存在: ${HASH_FILE}` };
  let lineno = 0;
  for (const raw of readFileSync(HASH_FILE, "utf8").split(/\r?\n/)) {
    lineno++;
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const m = line.match(/^([0-9a-f]{64})\s+\*?(.+)$/i);
    if (!m) {
      return { entries, error: `${HASH_FILE}:${lineno} 无法解析（应为 "<64位hex>  vendored/<path>"）: ${line}` };
    }
    entries.set(m[2].replace(/\\/g, "/"), m[1].toLowerCase());
  }
  return { entries, error: null };
}

function verifyVendoredHashes() {
  const { entries, error } = readHashBaseline();
  const problems = [];
  if (error) {
    problems.push({ kind: "baseline", detail: error });
    return { checked: entries.size, problems };
  }
  const onDisk = new Set(listVendoredFiles());
  for (const rel of [...entries.keys()].sort()) {
    if (!onDisk.has(rel)) {
      problems.push({ kind: "missing", rel, expected: entries.get(rel), actual: null });
      continue;
    }
    const actual = sha256Of(join(HERE, rel));
    if (actual !== entries.get(rel)) problems.push({ kind: "mismatch", rel, expected: entries.get(rel), actual });
    onDisk.delete(rel);
  }
  for (const rel of [...onDisk].sort()) {
    problems.push({ kind: "extra", rel, expected: null, actual: sha256Of(join(HERE, rel)) });
  }
  return { checked: entries.size, problems };
}

function reportHashProblems(problems) {
  console.error(`\npir-node-tests: vendored 完整性校验失败（${problems.length} 个问题）\n`);
  for (const p of problems) {
    if (p.kind === "baseline") {
      console.error(`  [基线]    ${p.detail}`);
    } else if (p.kind === "missing") {
      console.error(`  [缺失]    ${p.rel}\n            期望: ${p.expected}\n            实际: (文件不存在)`);
    } else if (p.kind === "extra") {
      console.error(`  [未登记]  ${p.rel}\n            期望: (不在 vendored.sha256 中)\n            实际: ${p.actual}`);
    } else {
      console.error(`  [不一致]  ${p.rel}\n            期望: ${p.expected}\n            实际: ${p.actual}`);
    }
  }
  console.error(`\n  ${HASH_HINT}\n`);
}

if (CHECK_HASHES_ONLY) {
  const v = verifyVendoredHashes();
  if (v.problems.length) {
    reportHashProblems(v.problems);
    console.error("pir-node-tests: --check-hashes 失败。");
    process.exit(1);
  }
  console.log(`pir-node-tests: vendored 完整性 OK — ${v.checked} 个文件与 vendored.sha256 全部一致。`);
  process.exit(0);
}

// Files intentionally not run, with the reason (Deno's config.jsonc analog).
export const EXCLUDED = {
  "test-path-resolve.js":
    "基础设施: 需要 child_process 自举 process.execPath + fixtures/path-resolve.js（pir 下 execPath 再入 pir 无法按 node 语义执行）",
  "test-events-once.js":
    "内部符号: 直接读 AbortSignal 的 EventTarget 内部存储 kEvents（需 --expose-internals），pir 的监听器存储无法对齐到该符号",
};

// ---------------------------------------------------------------------------
// require rewrites (applied to every vendored file)
// ---------------------------------------------------------------------------
const REWRITES = [
  // test/parallel files reach the shared infrastructure via '../common'.
  [/require\((['"])\.\.\/common\1\)/g, "require('./common/index.js')"],
  [/require\((['"])\.\.\/common\/fixtures\1\)/g, "require('./common/fixtures.js')"],
  // test/common sub-helpers the subset requires directly (tmpdir, hijackstdio,
  // …): stageWorkDir() copies the vendored ones next to index.js, so the
  // specifier is re-rooted the same way. One upstream file
  // (test-fs-readfile.js) spells the path as '../../test/common/<x>'.
  [/require\((['"])\.\.\/common\/([\w.-]+)\1\)/g, "require('./common/$2.js')"],
  [/require\((['"])\.\.\/\.\.\/test\/common\/([\w.-]+)\1\)/g, "require('./common/$2.js')"],
  // node:test → harness shim (pir's own node:test is throws-tier).
  [/require\((['"])node:test\1\)/g, "require('./shims/test.cjs')"],
  [/require\((['"])test\1\)/g, "require('./shims/test.cjs')"],
  // Node-internal test-only bindings.
  [/require\((['"])internal\/test\/binding\1\)/g, "require('./shims/internal_test_binding.cjs')"],
  [/require\((['"])internal\/event_target\1\)/g, "require('./shims/internal_event_target.cjs')"],
];

// Files whose failure is caused by the harness itself, not by pir — verified
// by an equal failure on the system-node control pass. Counted separately
// and removed from the compatibility denominator (Deno's `expected failure`
// config analog).
export const HARNESS_EXPECTED_FAIL = {
  "test-util-inspect.js":
    "harness: internalBinding('js_stream').JSStream 的 _externalStream 是 C++ External 指针，纯 JS stub 无法复现（控制组 node 同样失败）",
};

// ---------------------------------------------------------------------------
// common/index.js patches — recorded deviations, all infrastructure-level.
// Mirrors Deno's approach of adapting the suite rather than the runtime.
// ---------------------------------------------------------------------------
const COMMON_PATCHES = [
  {
    // pir reports no openssl in process.versions although node:crypto works;
    // hasCrypto must reflect the working crypto shim, or buffer tests would
    // silently take the no-crypto paths.
    from: /Boolean\(process\.versions\.openssl\)/g,
    to: "Boolean(process.versions.openssl ?? true)",
    why: "process.versions.openssl 未定义但 node:crypto 可用",
  },
  {
    // process.config exists in pir but has no `variables` object.
    from: /process\.config\.variables/g,
    to: "(process.config.variables || {})",
    why: "process.config.variables 未实现（hasQuic 探测）",
  },
  {
    // util.getCallSites exists in pir, but if it ever returns fewer than 2
    // frames the mustCall/mustNotCall failure reporter would crash instead
    // of reporting.
    from: /getCallSites\(\)\[1\]/g,
    to: "(getCallSites()[1] || { scriptName: '(unknown)', lineNumber: 0 })",
    why: "getCallSites 帧数不确定，防止 mustCall 失败报告自身崩溃",
  },
];

// ---------------------------------------------------------------------------
// common/**: the vendored upstream infrastructure copied into work/common/
// next to the transformed tests. Every file below is vendored byte-exact from
// the same tag (see MANIFEST.txt).
// ---------------------------------------------------------------------------
const COMMON_FILES = [
  "index.js",       // the shared test harness (mustCall, expectsError, skip, …)
  "fixtures.js",    // fixture-path helpers (used by the subset's require rewrite)
  "tmpdir.js",      // per-test temp directory (test-fs-*, test-module-cache, …)
  "hijackstdio.js", // stdout/stderr hijacking (test-console-*, test-process-warning)
];

// Staged common/*.js helpers live one level DEEPER than the test files
// (<work>/test/parallel/common/), so their own self-references must resolve to
// a sibling instead of re-entering ./common/. Applied only to common files.
//   ../common/<x>  → ./<x>.js      (shape used by the not-yet-vendored
//                                   crypto.js/gc.js/countdown.js helpers)
//   ../common      → ./index.js
//   ./             → ./index.js    (avoids relying on directory-index
//                                   resolution in pir)
const COMMON_SELF_REWRITES = [
  [/require\((['"])\.\.\/common\/([\w.-]+)\1\)/g, "require('./$2.js')"],
  [/require\((['"])\.\.\/common\1\)/g, "require('./index.js')"],
  [/require\((['"])\.\/\1\)/g, "require('./index.js')"],
];

// ---------------------------------------------------------------------------
// runner mechanics
// ---------------------------------------------------------------------------
function findPir() {
  const flagIndex = process.argv.indexOf("--pir");
  if (flagIndex !== -1 && process.argv[flagIndex + 1]) return process.argv[flagIndex + 1];
  if (process.env.PIR_EXE) return process.env.PIR_EXE;
  for (const c of [
    // debug first per the working directive: release rebuilds cost ~10 min
    // and the pass/fail verdicts are identical. A stale release binary would
    // make the whole report describe code that is no longer in the tree.
    join(REPO, "target-alt/debug/pir.exe"),
    join(REPO, "target-alt/release/pir.exe"),
    join(REPO, "target/debug/deps/pir.exe"),
    join(REPO, "target/release/pir.exe"),
    join(REPO, "target/debug/pir.exe"),
  ]) if (existsSync(c)) return c;
  throw new Error("pir binary not found; pass --pir <path>");
}

const useNode = process.argv.includes("--node");
const quiet = process.argv.includes("--quiet");
const onlyArg = (() => {
  const i = process.argv.indexOf("--only");
  return i !== -1 ? new Set(process.argv[i + 1].split(",").map((s) => s.trim())) : null;
})();
const outArg = (() => {
  const i = process.argv.indexOf("--out");
  if (i !== -1 && process.argv[i + 1]) return process.argv[i + 1];
  // A `--only` run is a partial measurement: keep the canonical REPORT.md
  // for full-suite runs so ad-hoc subset runs cannot clobber it.
  return onlyArg ? join(HERE, "REPORT.partial.md") : join(HERE, "REPORT.md");
})();

const RUNNER = useNode ? process.execPath : findPir();
const TIMEOUT_MS = 20_000;

// Per-file budget overrides for tests that are pure engine throughput rather
// than behavior, where QuickJS's allocation/scan rate (≈10x slower than V8) is
// the only thing being measured. Measured on this host, target-alt/debug:
//   test-performance-many-marks.js: 1e6 marks in a tight loop — 17.2s loop +
//   1.1s getEntriesByName + 0.4s clearMarks ≈ 19s, right at the 20s default.
//   test-buffer-indexof.js: 1e6 indexOf calls plus a 1 MB pattern search —
//   completes in 26s. (A 60s probe run of
//   test-async-hooks-execution-async-resource.js still hits the wall, so that
//   one keeps the default: it is a genuine hang, not throughput.)
// The assertions still have to pass; only the wall clock is relaxed.
const PER_FILE_TIMEOUT_MS = {
  "test-performance-many-marks.js": 60_000,
  "test-buffer-indexof.js": 90_000,
};
function timeoutFor(rel) {
  return PER_FILE_TIMEOUT_MS[rel] ?? TIMEOUT_MS;
}

// `pir js <file>` must be the raw script runner (real stdio + exit codes).
// On builds without js_mode the same argv falls through to agent/print mode,
// where an LLM "runs" the file — results would be meaningless. Probe once.
function probePirScriptMode() {
  const probePath = join(WORK, "probe.cjs");
  mkdirSync(WORK, { recursive: true });
  writeFileSync(probePath, 'console.log("PIR_JS_PROBE_OK");');
  const r = spawnSync(RUNNER, ["js", probePath], {
    cwd: WORK, encoding: "utf8", timeout: 15_000,
    env: PIR_ENV, stdio: ["ignore", "pipe", "pipe"],
  });
  if (!(r.stdout || "").includes("PIR_JS_PROBE_OK")) {
    throw new Error(
      `${RUNNER} does not have the raw 'pir js' script runner (fell through to agent mode?). ` +
      "Build a binary with js_mode (target-alt/* or target/debug/deps/pir.exe) or pass --pir.",
    );
  }
}

const PIR_ENV = {
  ...process.env,
  // test/common/index.js reads these (Deno sets the same pair):
  NODE_SKIP_FLAG_CHECK: "1",
  NODE_TEST_KNOWN_GLOBALS: "0",
  NO_COLOR: "1",
  PI_EXT_RELOAD: "1",
  PIR_AGENT_DIR: join(process.env.TEMP || process.env.TMP || ".", "pir-node-tests", "agent-dir"),
};

function listTestFiles() {
  const files = readFileSync(join(HERE, "MANIFEST.txt"), "utf8")
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith("#"));
  return files;
}

function transform(rel) {
  let src = readFileSync(join(VENDOR, rel), "utf8");
  const patches = [];
  for (const [rx, to] of REWRITES) {
    const before = src;
    src = src.replace(rx, to);
    if (src !== before) patches.push(rewriteLabel(to) + " 改写");
  }
  if (rel === "common/index.js") {
    const patched = applyCommonPatches(src);
    src = patched.src;
    patches.push(...patched.applied);
  }
  return { src, patches };
}

// Human-readable label for the report's "本轮改写" list: the destination
// specifier with any $1/$2 capture references shown as `*`.
function rewriteLabel(to) {
  const dest = (String(to).match(/require\((['"])([^'"]+)\1\)/) || [])[2] || String(to);
  return dest.replace(/\$\d/g, "*");
}

function classify(res, isTestRunner) {
  const { code, stdout, stderr, timedOut, timeoutMs } = res;
  const out = stdout || "";
  const err = stderr || "";

  let counts = null;
  const marker = out.match(/__NODERESULT__ (\{.*\})/);
  if (marker) {
    try { counts = JSON.parse(marker[1]); } catch { /* fall through */ }
  }

  const mustCallAuditFail = /Mismatched .* function calls/.test(out);
  const pass =
    code === 0 &&
    !timedOut &&
    !mustCallAuditFail &&
    (!isTestRunner || (counts && counts.fail === 0));

  if (pass) return { verdict: "pass", counts, category: null, why: null };

  let category = "其他";
  let why = firstErrLine(err || out);
  if (timedOut) { category = "timeout"; why = `${(timeoutMs || TIMEOUT_MS) / 1000}s 超时（事件循环未排空或挂起）`; }
  else if (/Cannot find module|Error loading module|ERR_MODULE_NOT_FOUND|ModuleNotFound/i.test(err + out)) category = "模块缺失";
  else if (/is not supported by the pir extension runtime/i.test(err + out)) category = "throws档API未实现";
  else if (/SyntaxError|Unexpected token|Invalid regular expression|invalid flag/i.test(err)) category = "语法特性不支持";
  else if (/AssertionError/.test(err)) category = "行为差异(assert)";
  else if (/not a function|has no property|is not a constructor|reading '[^']*' of undefined/i.test(err)) category = "API缺失";
  else if (/TypeError|ReferenceError|RangeError|InternalError/.test(err)) category = "API缺失或行为差异";
  else if (mustCallAuditFail) { category = "mustCall未满足"; why = (out.match(/.*function calls\. Expected.*$/) || [why])[0]; }
  else if (code !== 0 && isTestRunner && counts) { category = "行为差异(assert)"; }

  return { verdict: "fail", counts, category, why };
}

function firstErrLine(s) {
  const lines = (s || "").split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  const skip = /^(ok|not ok|#|__NODERESULT__|\[ext\]|.*INFO.*agent\.run)/;
  const errLine = lines.find((l) => !skip.test(l) && !l.startsWith("at ") &&
    /^(Error|AssertionError|TypeError|ReferenceError|RangeError|SyntaxError|InternalError|node:test shim|\[ERR_)/.test(l));
  return (errLine || lines[0] || "").slice(0, 220);
}

function runOne(rel) {
  const { src, patches } = transform(rel);
  // Keep the original file name: several tests assert on `__filename`, and
  // a .js file without a package.json "type" runs as CJS on both engines.
  const outPath = join(WORK, basename(rel));
  mkdirSync(WORK, { recursive: true });
  writeFileSync(outPath, src, "utf8");

  const cmd = useNode ? RUNNER : RUNNER;
  const args = useNode ? [outPath] : ["js", outPath];
  const r = spawnSync(cmd, args, {
    cwd: WORK,
    encoding: "utf8",
    timeout: timeoutFor(rel),
    env: useNode ? { ...process.env, NODE_SKIP_FLAG_CHECK: "1", NODE_TEST_KNOWN_GLOBALS: "0", NO_COLOR: "1" } : PIR_ENV,
    stdio: ["ignore", "pipe", "pipe"],
  });
  const timedOut = r.error && r.error.code === "ETIMEDOUT";
  const isTestRunner = src.includes("shims/test.cjs");
  const res = { code: r.status, stdout: r.stdout || "", stderr: r.stderr || "", timedOut, timeoutMs: timeoutFor(rel) };
  const cls = classify(res, isTestRunner);
  return { rel, outPath, patches, ...cls, exitCode: r.status, stderrTail: (r.stderr || "").slice(-1500) };
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------
function applyCommonPatches(src) {
  const applied = [];
  for (const p of COMMON_PATCHES) {
    const before = src;
    src = src.replace(p.from, p.to);
    if (src !== before) applied.push(p.why);
  }
  return { src, applied };
}

function stageWorkDir() {
  rmSync(join(HERE, "work"), { recursive: true, force: true });
  mkdirSync(join(WORK, "common"), { recursive: true });
  mkdirSync(join(WORK, "shims"), { recursive: true });
  for (const f of COMMON_FILES) {
    let src = readFileSync(join(VENDOR, "common", f), "utf8");
    for (const [rx, to] of COMMON_SELF_REWRITES) src = src.replace(rx, to);
    if (f === "index.js") src = applyCommonPatches(src).src;
    writeFileSync(join(WORK, "common", f), src);
  }
  for (const f of ["test.cjs", "internal_test_binding.cjs", "internal_event_target.cjs"]) {
    writeFileSync(join(WORK, "shims", f), readFileSync(join(SHIMS, f)));
  }
}

// ---------------------------------------------------------------------------
// run lock: concurrent runners all stage into the single `work/` tree, and
// the `rmSync` in stageWorkDir would wipe another run's staged files
// mid-flight (observed as swaths of "module not found"). Serialize whole
// runs; a crashed runner's lock goes stale after LOCK_STALE_MS.
// ---------------------------------------------------------------------------
const RUN_LOCK = join(HERE, "work.lock");
const LOCK_WAIT_LIMIT_MS = 15 * 60_000;
const LOCK_STALE_MS = 30 * 60_000;

function sleepSync(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function acquireRunLock() {
  const deadline = Date.now() + LOCK_WAIT_LIMIT_MS;
  let announced = false;
  for (;;) {
    try {
      const fd = openSync(RUN_LOCK, "wx");
      writeSync(fd, String(process.pid));
      closeSync(fd);
      process.on("exit", () => {
        try {
          rmSync(RUN_LOCK, { force: true });
        } catch {
          // best-effort release
        }
      });
      return;
    } catch (error) {
      if (error.code !== "EEXIST") throw error;
      let age = 0;
      try {
        age = Date.now() - statSync(RUN_LOCK).mtimeMs;
      } catch {
        continue; // lock vanished between open and stat
      }
      if (age > LOCK_STALE_MS) {
        rmSync(RUN_LOCK, { force: true });
        continue;
      }
      if (Date.now() > deadline) {
        throw new Error(
          "another node_tests runner is still running (" + RUN_LOCK + " held); retry later"
        );
      }
      if (!announced && !quiet) {
        console.log("pir-node-tests: another runner holds the work lock; waiting…");
        announced = true;
      }
      sleepSync(3000);
    }
  }
}

function main() {
  // 完整性门：先校验 vendored/**（控制组 --node 走同一条路径，一并受门），
  // 任何不匹配都在 stage work/ 之前响亮失败。
  if (SKIP_HASH_CHECK) {
    console.error("pir-node-tests: 警告: 已跳过 vendored 完整性校验（--skip-hash-check，仅限本地诊断）！");
  } else {
    const v = verifyVendoredHashes();
    if (v.problems.length) {
      reportHashProblems(v.problems);
      console.error("pir-node-tests: 拒绝运行 — 先把 vendored/ 恢复为基线原样（或从上游重取）再复跑。");
      process.exit(1);
    }
    if (!quiet) console.log(`pir-node-tests: vendored 完整性 OK — ${v.checked} 个文件与 vendored.sha256 一致。`);
  }
  acquireRunLock();
  mkdirSync(PIR_ENV.PIR_AGENT_DIR, { recursive: true });
  stageWorkDir();
  if (!useNode) probePirScriptMode();
  // common/* entries are staged infrastructure, not scored test files.
  const manifest = listTestFiles().filter((f) => !f.startsWith("common/"));
  const runnable = manifest.filter((f) => !EXCLUDED[f]);
  const selected = onlyArg ? runnable.filter((f) => onlyArg.has(f.replace(/\.js$/, "")) || onlyArg.has(f)) : runnable;
  const engine = useNode ? `node ${process.version}（控制组）` : `pir js @ ${RUNNER}`;

  console.log(`pir-node-tests: ${selected.length}/${manifest.length} files, engine=${engine}`);

  const results = [];
  for (const rel of selected) {
    if (!existsSync(join(VENDOR, rel))) {
      results.push({ rel, verdict: "missing", category: "清单缺文件", why: "vendored/ 下不存在" });
      continue;
    }
    const res = runOne(rel);
    if (res.verdict === "fail" && HARNESS_EXPECTED_FAIL[rel]) {
      res.verdict = "harness-fail";
      res.category = "harness预期失败";
      res.why = HARNESS_EXPECTED_FAIL[rel];
    }
    results.push(res);
    const tag = res.verdict === "pass" ? "PASS" : "FAIL";
    const extra = res.counts ? ` [p${res.counts.pass}/f${res.counts.fail}/s${res.counts.skipped}]` : "";
    if (!quiet) console.log(`[${tag}] ${rel}${extra}${res.category ? " — " + res.category + ": " + res.why : ""}`);
  }

  writeReport(results, manifest, engine);
  const pass = results.filter((r) => r.verdict === "pass").length;
  const fail = results.filter((r) => r.verdict === "fail").length;
  const hf = results.filter((r) => r.verdict === "harness-fail").length;
  console.log(`\nSUMMARY: files=${results.length} pass=${pass} fail=${fail} harnessFail=${hf} excluded=${Object.keys(EXCLUDED).length}`);
  console.log(`report: ${outArg}`);
}

function writeReport(results, manifest, engine) {
  const pass = results.filter((r) => r.verdict === "pass");
  const fail = results.filter((r) => r.verdict === "fail");
  const harnessFail = results.filter((r) => r.verdict === "harness-fail");
  const missing = results.filter((r) => r.verdict === "missing");
  const denom = pass.length + fail.length; // harness-fail 不计入分母
  const pct = (n, d) => (d ? ((100 * n) / d).toFixed(1) + "%" : "n/a");

  const subtests = results.filter((r) => r.counts);
  const subPass = subtests.reduce((a, r) => a + r.counts.pass, 0);
  const subFail = subtests.reduce((a, r) => a + r.counts.fail, 0);
  const subSkip = subtests.reduce((a, r) => a + r.counts.skipped + r.counts.todo, 0);

  const byCat = {};
  for (const f of fail) byCat[f.category] = (byCat[f.category] || 0) + 1;

  const lines = [];
  lines.push("# pir × Node 官方测试套件（子集）兼容率报告");
  lines.push("");
  lines.push(`- 日期: ${new Date().toISOString().slice(0, 10)} · 引擎: \`${engine}\``);
  lines.push(`- 来源: nodejs/node \`${NODE_TAG}\` test/parallel，vendored 原样保存（仅 require 改写 + ${COMMON_PATCHES.length} 处 common 补丁）`);
  lines.push(`- 文件级: ${pass.length}/${denom} 通过 (${pct(pass.length, denom)})；fail ${fail.length}；harness 预期失败 ${harnessFail.length}${missing.length ? "；清单缺文件 " + missing.length : ""}`);
  if (subtests.length) {
    lines.push(`- 子测试级（node:test 文件）: pass ${subPass} / fail ${subFail} / skip ${subSkip} → 通过率 ${pct(subPass, subPass + subFail)}`);
  }
  lines.push(`- 排除（不运行）: ${Object.keys(EXCLUDED).length}`);
  lines.push("");
  lines.push("## 逐文件结果");
  lines.push("");
  lines.push("| 文件 | 结果 | 子测试 p/f/s | 失败归类 | 首个错误 |");
  lines.push("|---|---|---|---|---|");
  for (const r of results) {
    const counts = r.counts ? `${r.counts.pass}/${r.counts.fail}/${r.counts.skipped + r.counts.todo}` : "—";
    const why = (r.verdict === "pass" ? "" : (r.why || "")).replace(/\|/g, "\\|");
    lines.push(`| ${r.rel} | ${r.verdict} | ${counts} | ${r.category || ""} | ${why} |`);
  }
  lines.push("");
  lines.push("## 失败归类统计");
  lines.push("");
  for (const [cat, n] of Object.entries(byCat).sort((a, b) => b[1] - a[1])) {
    lines.push(`- ${cat}: ${n}`);
  }
  if (!fail.length) lines.push("- （无失败）");
  lines.push("");
  lines.push("## 排除清单（不运行）");
  lines.push("");
  for (const [f, why] of Object.entries(EXCLUDED)) lines.push(`- ${f}: ${why}`);
  lines.push("");
  if (harnessFail.length) {
    lines.push("## harness 预期失败（不计入分母）");
    lines.push("");
    for (const r of harnessFail) lines.push(`- ${r.rel}: ${r.why}`);
    lines.push("");
  }
  const failedFiles = results.filter((r) => r.verdict === "fail");
  if (failedFiles.length) {
    lines.push("## 失败样本 stderr 尾部");
    lines.push("");
    for (const r of failedFiles) {
      lines.push(`### ${r.rel} — ${r.category}`);
      lines.push("```");
      lines.push((r.stderrTail || "(empty)").trim().slice(-1200) || "(empty)");
      lines.push("```");
      lines.push("");
    }
  }
  lines.push("## 本轮改写/补丁清单");
  lines.push("");
  const patchSet = new Set(results.flatMap((r) => r.patches));
  for (const p of patchSet) lines.push(`- ${p}`);
  lines.push("");

  writeFileSync(outArg, lines.join("\n") + "\n", "utf8");
  writeFileSync(join(HERE, "work", "results.json"), JSON.stringify(results, null, 1), "utf8");
}

main();
