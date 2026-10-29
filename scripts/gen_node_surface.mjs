// gen_node_surface.mjs — node-surface name-surface generator (roadmap §8/§11.3).
//
// Dumps, for every shimmed node builtin, the export names of the REAL node
// binary (ground truth, via `Object.keys(require(module))`) against the names
// the pir shim actually exposes at runtime (via `pir js` + require of the same
// specifiers), and prints the per-module diff report.
//
// Usage:  node scripts/gen_node_surface.mjs [--pir <path-to-pir.exe>]
// Exit code is 0 even when gaps exist; the report is the artifact.
// Output also lands in %TEMP%/pir-compat/node-surface-diff.txt.
//
// Hygiene note: `.mjs` is deliberately NOT on the forbidden-files list
// (tests/suites/forbidden_files.rs bans `.ts/.tsx/.js`, `package.json` and
// `node_modules` only), so this generator can live in `scripts/` as-is.

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = join(dirname(fileURLToPath(import.meta.url)), "..");
const NODE_SHIMS_RS = join(REPO, "src/extensions/api/node_shims.rs");
const OUT_DIR = join(process.env.TEMP || process.env.TMP || ".", "pir-compat");

function findPir() {
  const flagIndex = process.argv.indexOf("--pir");
  if (flagIndex !== -1 && process.argv[flagIndex + 1]) return process.argv[flagIndex + 1];
  const candidates = [
    process.env.PIR_EXE,
    join(REPO, "target-alt/debug/pir.exe"),
    join(REPO, "target-alt/debug/pir"),
    join(REPO, "target/debug/pir.exe"),
    join(REPO, "target/debug/pir"),
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (existsSync(candidate)) return candidate;
  }
  throw new Error("pir binary not found; pass --pir <path> or build first");
}

// Parse the NODE_KNOWN table out of node_shims.rs (single source of truth).
function shimmedModules() {
  const src = readFileSync(NODE_SHIMS_RS, "utf8");
  const tableStart = src.indexOf("const NODE_KNOWN");
  const tableEnd = src.indexOf("];", tableStart);
  if (tableStart < 0 || tableEnd < 0) throw new Error("NODE_KNOWN table not found");
  const table = src.slice(tableStart, tableEnd);
  const modules = [];
  const rx = /\("([^"]+)",\s*"@node\/([^"]+)"\)/g;
  let m;
  while ((m = rx.exec(table))) modules.push({ specifier: m[1], canonical: m[2] });
  return modules;
}

// Unique canonical short names (aliases like `_stream_duplex` map onto
// public shims; the runtime surface to audit is the unique @node/<name> set).
function uniqueCanonicals(modules) {
  const seen = new Set();
  const out = [];
  for (const mod of modules) {
    if (seen.has(mod.canonical)) continue;
    seen.add(mod.canonical);
    out.push(mod.canonical);
  }
  return out;
}

function realNodeExports(moduleName) {
  try {
    const json = execFileSync(
      process.execPath,
      ["-e", `process.stdout.write(JSON.stringify(Object.keys(require('node:' + ${JSON.stringify(moduleName)}))))`],
      { encoding: "utf8", timeout: 15000, stdio: ["ignore", "pipe", "pipe"] },
    );
    return JSON.parse(json);
  } catch (error) {
    return { __error: String(error.stderr || error.message).split("\n")[0] };
  }
}

function pirExports(pirExe, moduleNames) {
  // One `pir js` process dumps every module's runtime keys.
  const lines = [
    "const results = {};",
    "for (const name of " + JSON.stringify(moduleNames) + ") {",
    "  try {",
    "    const ns = require('node:' + name);",
    "    results[name] = Object.keys(ns);",
    "  } catch (error) {",
    "    results[name] = { __error: String(error && error.message ? error.message : error) };",
    "  }",
    "}",
    "console.log(JSON.stringify(results));",
  ];
  const fixture = join(OUT_DIR, "pir-surface-dump.cjs");
  writeFileSync(fixture, lines.join("\n"), "utf8");
  const stdout = execFileSync(pirExe, ["js", fixture], {
    encoding: "utf8",
    timeout: 60000,
    env: { ...process.env, PIR_AGENT_DIR: join(OUT_DIR, "pir-surface-agent-dir") },
  });
  const jsonLine = stdout.split(/\r?\n/).reverse().find((line) => line.startsWith("{"));
  if (!jsonLine) throw new Error("pir js produced no JSON line: " + stdout.slice(0, 400));
  return JSON.parse(jsonLine);
}

const modules = shimmedModules();
const names = uniqueCanonicals(modules);
mkdirSync(OUT_DIR, { recursive: true });

console.log(`auditing ${names.length} shimmed node builtins against real node ${process.version}...`);
const real = {};
for (const name of names) real[name] = realNodeExports(name);
const pir = pirExports(findPir(), names);

const report = [];
let totalMissing = 0;
let totalExtra = 0;
for (const name of names) {
  const realEntry = real[name];
  const pirEntry = pir[name];
  if (realEntry && realEntry.__error) {
    report.push(`## ${name}\n  real node: <unavailable: ${realEntry.__error}>`);
    continue;
  }
  if (!pirEntry || pirEntry.__error) {
    report.push(`## ${name}\n  pir: <load failed: ${pirEntry ? pirEntry.__error : "<missing>"}>`);
    continue;
  }
  const realKeys = new Set(realEntry);
  const pirKeys = new Set(pirEntry);
  // `default` is an ESM-namespace artifact of pir's CJS require (the shim
  // modules export a default object) — reported separately, not a gap.
  const missing = realEntry.filter((k) => !pirKeys.has(k));
  const extra = pirEntry.filter((k) => !realKeys.has(k) && k !== "default");
  const artifact = pirKeys.has("default") ? 1 : 0;
  totalMissing += missing.length;
  totalExtra += extra.length;
  if (missing.length || extra.length) {
    report.push(`## ${name} (${missing.length} missing, ${extra.length} extra${artifact ? ", +default artifact" : ""})`);
    if (missing.length) report.push("  missing: " + missing.join(", "));
    if (extra.length) report.push("  extra:   " + extra.join(", "));
  } else {
    report.push(`## ${name}\n  OK (${pirEntry.length - artifact} names${artifact ? ", +default artifact" : ""})`);
  }
}

const summary =
  `node-surface diff — real node ${process.version} vs pir shims\n` +
  `modules audited: ${names.length}, missing names: ${totalMissing}, extra names: ${totalExtra}\n\n` +
  report.join("\n");
console.log("\n" + summary);
const outPath = join(OUT_DIR, "node-surface-diff.txt");
writeFileSync(outPath, summary + "\n", "utf8");
console.log(`\nreport written to ${outPath}`);
