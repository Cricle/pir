// gen_pi_surface.mjs — pi SDK value-export surface audit (pir vs upstream pi).
//
// For each upstream package (`tui`, `coding-agent`, `ai`, `agent`, `codemode`):
//   1. recursively parse `src/index.ts` (export * chains, `.ts`/`.js` specifier
//      mapping, type-only entries filtered) to get the upstream VALUE export
//      set (types/interfaces excluded — they have no runtime key);
//   2. enumerate what the pir virtual shim really exposes at runtime by
//      importing it in REAL node (shims are plain JS; only `@node/*` needs the
//      `node:*` rewrite) with a synthetic node_modules tree;
//   3. print the per-package diff (missing / extra) and write the report.
//
// Usage:  node scripts/gen_pi_surface.mjs [--upstream <pi-checkout>] [--out <dir>]
// Exit code stays 0; the report is the artifact.

import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";

const REPO = resolve(join(dirname(fileURLToPath(import.meta.url)), ".."));
const JS_DIR = join(REPO, "src/extensions/api/js");
const OUT_DIR = join(process.env.TEMP || process.env.TMP || ".", "pir-compat");

function argValue(flag, fallback) {
  const i = process.argv.indexOf(flag);
  return i !== -1 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const UPSTREAM = resolve(argValue("--upstream", "C:/Users/XPENG_USER/Desktop/github/pi"));
const OUT = resolve(argValue("--out", OUT_DIR));

// ---------------------------------------------------------------------------
// 1. Upstream TS value-export parser
// ---------------------------------------------------------------------------

const fileCache = new Map();

function stripCommentsAndStrings(src, keepLines) {
  // Replace // and /* */ comments and string/template bodies with spaces so
  // line-anchored export regexes can't trip on prose inside them.
  let out = "";
  let i = 0;
  const n = src.length;
  let state = "code";
  let quote = "";
  while (i < n) {
    const ch = src[i];
    const next = src[i + 1];
    if (state === "code") {
      if (ch === "/" && next === "/") {
        state = "line";
        i += 2;
        continue;
      }
      if (ch === "/" && next === "*") {
        state = "block";
        i += 2;
        continue;
      }
      if (ch === '"' || ch === "'" || ch === "`") {
        quote = ch;
        state = "string";
        out += ch;
        i += 1;
        continue;
      }
      out += ch;
      i += 1;
      continue;
    }
    if (state === "line") {
      if (ch === "\n") {
        state = "code";
        out += "\n";
      }
      i += 1;
      continue;
    }
    if (state === "block") {
      if (ch === "*" && next === "/") {
        state = "code";
        i += 2;
        continue;
      }
      if (ch === "\n") out += "\n";
      i += 1;
      continue;
    }
    // string / template: keep single/double-quoted bodies verbatim (export
    // specifiers live there); blank template literals (prose).
    if (ch === "\\") {
      out += quote === "`" ? "  " : src.slice(i, i + 2);
      i += 2;
      continue;
    }
    if (ch === quote) {
      state = "code";
      out += quote;
      i += 1;
      continue;
    }
    if (quote === "`" && ch === "$" && next === "{") {
      // Keep ${...} expressions as code so nested strings don't break the
      // scanner (export statements never live there; just balance braces).
      out += "  ";
      i += 2;
      let depth = 1;
      while (i < n && depth > 0) {
        if (src[i] === "{") depth += 1;
        else if (src[i] === "}") depth -= 1;
        out += src[i] === "\n" ? "\n" : " ";
        i += 1;
      }
      continue;
    }
    out += quote === "`" ? (ch === "\n" ? "\n" : " ") : ch;
    i += 1;
  }
  if (!keepLines) return out;
  return out;
}

function resolveSpecifier(fromFile, spec) {
  if (!spec.startsWith(".")) return null; // external package
  const base = join(dirname(fromFile), spec);
  const candidates = [];
  const addTs = (p) => {
    candidates.push(p);
    if (p.endsWith(".js")) candidates.push(p.slice(0, -3) + ".ts");
    if (p.endsWith(".mjs")) candidates.push(p.slice(0, -4) + ".mts");
    if (!/\.(ts|mts|cts|js|mjs|cjs)$/.test(p)) {
      candidates.push(p + ".ts", p + ".tsx", join(p, "index.ts"));
    }
  };
  addTs(base);
  for (const c of candidates) if (existsSync(c) && !c.endsWith("/")) return c;
  return null;
}

function parseExportList(listText, specifier, targetExports, local) {
  const names = [];
  for (let entry of listText.split(",")) {
    entry = entry.trim();
    if (!entry) continue;
    let typeOnly = false;
    if (/^type\s/.test(entry)) {
      typeOnly = true;
      entry = entry.replace(/^type\s+/, "").trim();
    }
    let name = entry;
    const asMatch = entry.match(/\s+as\s+([A-Za-z_$][\w$]*)$/);
    if (asMatch) name = asMatch[1];
    if (!/^[A-Za-z_$][\w$]*$/.test(name)) continue;
    if (typeOnly) continue;
    if (local && !specifier) {
      if (local.types.has(name) && !local.values.has(name)) continue;
    } else if (targetExports && targetExports.has(name)) {
      if (targetExports.get(name) === "type") continue;
    }
    names.push(name);
  }
  return names;
}

function analyze(file) {
  if (fileCache.has(file)) return fileCache.get(file);
  const entry = { values: new Set(), types: new Set(), map: new Map() };
  fileCache.set(file, entry); // pre-seed to break cycles

  const src = stripCommentsAndStrings(readFileSync(file, "utf8"), true);
  const lines = src.split(/\r?\n/);

  // Pass 1: local declarations (value vs type).
  const localClass = { types: new Set(), values: new Set() };
  const declRx =
    /^(?:\s*)(?:export\s+)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(const|let|var|function|class|enum|type|interface)\s+([A-Za-z_$][\w$]*)/;
  const enumMemberDeclRx = /^(\s*)(?:export\s+)?(?:declare\s+)?(const\s+)?enum\s+([A-Za-z_$][\w$]*)/;
  for (const line of lines) {
    const m = line.match(declRx);
    if (!m) continue;
    const kind = m[1];
    const name = m[2];
    const isType = kind === "type" || kind === "interface";
    if (isType) {
      entry.types.add(name);
      localClass.types.add(name);
    } else {
      entry.values.add(name);
      localClass.values.add(name);
      // `export const enum X` also lands here (kind === "const" via regex); the
      // enum-member regex below is a no-op for plain constants.
      void enumMemberDeclRx;
    }
  }

  // Pass 2: export statements.
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!/^export\b/.test(line)) {
      i += 1;
      continue;
    }
    if (/^export\s+default\b/.test(line)) {
      entry.map.set("default", "value");
      i += 1;
      continue;
    }
    // export * from "x" / export * as ns from "x"
    let m = line.match(/^export\s+\*\s+as\s+([A-Za-z_$][\w$]*)\s+from\s+["']([^"']+)["']/);
    if (m) {
      entry.map.set(m[1], "value");
      i += 1;
      continue;
    }
    m = line.match(/^export\s+\*\s+from\s+["']([^"']+)["']/);
    if (m) {
      const target = resolveSpecifier(file, m[1]);
      if (target) {
        const sub = analyze(target);
        for (const [name, kind] of sub.map) entry.map.set(name, kind);
      }
      i += 1;
      continue;
    }
    // export type { ... } — whole statement is type-only
    if (/^export\s+type\s*\{/.test(line)) {
      i = skipBalanced(lines, i);
      continue;
    }
    // export { ... } [from "x"]
    if (/^export\s*\{/.test(line)) {
      const { text, endLine, rest } = collectExportList(lines, i);
      i = endLine + 1;
      const fromMatch = rest.match(/from\s+["']([^"']+)["']/);
      const specifier = fromMatch ? fromMatch[1] : null;
      let targetExports = null;
      if (specifier) {
        const target = resolveSpecifier(file, specifier);
        if (target) targetExports = analyze(target).map;
      }
      for (const name of parseExportList(text, specifier, targetExports, localClass)) {
        entry.map.set(name, "value");
      }
      continue;
    }
    // export const/function/class/enum ...
    m = line.match(
      /^export\s+(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(const|let|var|function|class|enum)\s+([A-Za-z_$][\w$]*)/,
    );
    if (m) {
      entry.map.set(m[2], "value");
      i += 1;
      continue;
    }
    i += 1;
  }

  // Local declarations are only *exported* when listed in an export statement,
  // except when declared with a leading `export` keyword — pass 2 covers that.
  fileCache.set(file, entry);
  return entry;
}

function skipBalanced(lines, startLine) {
  let depth = 0;
  for (let i = startLine; i < lines.length; i += 1) {
    for (const ch of lines[i]) {
      if (ch === "{") depth += 1;
      else if (ch === "}") {
        depth -= 1;
        if (depth === 0) return i + 1;
      }
    }
  }
  return lines.length;
}

function collectExportList(lines, startLine) {
  let text = "";
  let depth = 0;
  let started = false;
  for (let i = startLine; i < lines.length; i += 1) {
    const line = lines[i];
    for (let j = 0; j < line.length; j += 1) {
      const ch = line[j];
      if (ch === "{") {
        depth += 1;
        if (depth === 1) {
          started = true;
          continue;
        }
      }
      if (ch === "}") {
        depth -= 1;
        if (depth === 0) {
          return { text, endLine: i, rest: line.slice(j + 1) };
        }
      }
      if (started && depth >= 1) text += ch;
    }
    text += "\n";
  }
  return { text, endLine: lines.length - 1, rest: "" };
}

function upstreamValueExports(packageName) {
  const index = join(UPSTREAM, "packages", packageName, "src/index.ts");
  if (!existsSync(index)) return null;
  const entry = analyze(index);
  const names = new Set();
  for (const [name, kind] of entry.map) if (kind === "value") names.add(name);
  return names;
}

// ---------------------------------------------------------------------------
// 2. pir shim runtime surface (real node)
// ---------------------------------------------------------------------------

function rewriteShim(text) {
  return text.replace(/(["'])@node\//g, "$1node:");
}

// package entry -> shim file + subpath map (mirrors shims.rs::canonical_virtual_name).
const SHIM_PACKAGES = {
  "@earendil-works/pi-tui": {
    file: "pi_tui_shim.js.txt",
    upstreamDir: "tui",
    subpaths: { ".": "index.js" },
  },
  "@earendil-works/pi-coding-agent": {
    file: "pi_coding_agent_shim.js.txt",
    upstreamDir: "coding-agent",
    subpaths: { ".": "index.js", "./rpc-entry": "rpc-entry.js" },
  },
  "@earendil-works/pi-agent-core": {
    file: "pi_agent_core_shim.js.txt",
    upstreamDir: "agent",
    subpaths: { ".": "index.js" },
  },
  "@earendil-works/pi-ai": {
    file: "pi_ai_shim.js.txt",
    upstreamDir: "ai",
    subpaths: {
      ".": "index.js",
      "./compat": "index.js",
      "./models": "models.js",
      "./bedrock-provider": "bedrock-provider.js",
      "./bun-oauth": "bun-oauth.js",
      "./oauth": "oauth.js",
      "./providers/all": "providers-all.js",
    },
    extra: {
      "oauth.js": "pi_ai_oauth_shim.js.txt",
      "providers-all.js": "pi_ai_providers_all_shim.js.txt",
    },
  },
  "@earendil-works/pi-codemode": {
    file: "pi_codemode_shim.js.txt",
    upstreamDir: "codemode",
    subpaths: {
      ".": "index.js",
      "./declarations": "declarations.js",
      "./source": "source.js",
      "./worker": "worker.js",
      "./portable": "portable.js",
      "./cloudflare": "cloudflare.js",
    },
  },
  typebox: {
    file: "typebox_shim.js.txt",
    upstreamDir: null,
    subpaths: { ".": "index.js" },
  },
};

function buildShimTree(dir) {
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(join(dir, "node_modules"), { recursive: true });
  for (const [name, spec] of Object.entries(SHIM_PACKAGES)) {
    const pkgDir = join(dir, "node_modules", name);
    mkdirSync(pkgDir, { recursive: true });
    const exportsMap = {};
    for (const [sub, outFile] of Object.entries(spec.subpaths)) {
      exportsMap[sub] = "./" + outFile;
    }
    writeFileSync(
      join(pkgDir, "package.json"),
      JSON.stringify({ name, version: "0.0.0-pir", type: "module", main: "./index.js", exports: exportsMap }, null, 2),
    );
    writeFileSync(join(pkgDir, "index.js"), rewriteShim(readFileSync(join(JS_DIR, spec.file), "utf8")));
    // Every registered subpath resolves to a file: the primary shim unless an
    // explicit override is given (mirrors shims.rs::source_for).
    for (const [outFile, override] of Object.entries(spec.extra || {})) {
      writeFileSync(join(pkgDir, outFile), rewriteShim(readFileSync(join(JS_DIR, override), "utf8")));
    }
    for (const outFile of Object.values(spec.subpaths)) {
      if (outFile === "index.js" || (spec.extra || {})[outFile]) continue;
      writeFileSync(join(pkgDir, outFile), rewriteShim(readFileSync(join(JS_DIR, spec.file), "utf8")));
    }
  }
}

function pirSurface(dir, specifier) {
  const probe = `
import * as NS from ${JSON.stringify(specifier)};
const defaultKeys = (NS && NS.default && typeof NS.default === "object")
  ? Object.keys(NS.default) : [];
console.log("__RESULT__" + JSON.stringify({
  named: Object.keys(NS),
  defaultKeys,
  defaultType: typeof (NS && NS.default),
}));
`;
  const probeFile = join(dir, "probe-" + Math.random().toString(36).slice(2) + ".mjs");
  writeFileSync(probeFile, probe);
  try {
    const stdout = execFileSync(process.execPath, [probeFile], {
      encoding: "utf8",
      cwd: dir,
      timeout: 30000,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const line = stdout.split(/\r?\n/).find((l) => l.startsWith("__RESULT__"));
    if (!line) return { error: "no result line: " + stdout.slice(0, 300) };
    return JSON.parse(line.slice("__RESULT__".length));
  } catch (error) {
    return { error: String(error.stderr || error.message).split("\n").slice(0, 4).join(" | ") };
  } finally {
    rmSync(probeFile, { force: true });
  }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

const work = join(OUT, "pi-surface-node-modules");
mkdirSync(OUT, { recursive: true });
buildShimTree(work);

const report = [];
const upstreamLists = {};
let totals = { upstream: 0, pir: 0, missing: 0 };

for (const [pkg, spec] of Object.entries(SHIM_PACKAGES)) {
  const short = pkg.replace("@earendil-works/", "");
  const upstream = spec.upstreamDir ? upstreamValueExports(spec.upstreamDir) : null;
  const surface = pirSurface(work, pkg);
  if (surface.error) {
    report.push(`## ${short}\n  pir shim import FAILED: ${surface.error}`);
    continue;
  }
  const named = new Set(surface.named.filter((n) => n !== "default"));
  const defaultKeys = new Set(surface.defaultKeys);
  const pirAll = new Set([...named, ...defaultKeys]);
  if (!upstream) {
    report.push(
      `## ${short}\n  (no upstream audit target) pir named=${named.size} default=${defaultKeys.size}`,
    );
    continue;
  }
  const missing = [...upstream].filter((n) => !pirAll.has(n)).sort();
  const extra = [...pirAll].filter((n) => !upstream.has(n)).sort();
  const onlyDefault = [...upstream].filter((n) => named.has(n) && !defaultKeys.has(n)).sort();
  const onlyNamed = [...upstream].filter((n) => !named.has(n) && defaultKeys.has(n)).sort();
  upstreamLists[short] = [...upstream].sort();
  totals.upstream += upstream.size;
  totals.pir += pirAll.size;
  totals.missing += missing.length;
  report.push(
    `## ${short}: upstream=${upstream.size} pir=${pirAll.size} (named=${named.size}, default=${defaultKeys.size}) missing=${missing.length} extra=${extra.length}`,
  );
  if (missing.length) report.push("  MISSING: " + missing.join(", "));
  if (onlyDefault.length) report.push("  (named only, not in default object): " + onlyDefault.join(", "));
  if (onlyNamed.length) report.push("  (default only, not named): " + onlyNamed.join(", "));
  if (extra.length) report.push("  extra: " + extra.join(", "));
}

// ---------------------------------------------------------------------------
// 3. Subpath exports: upstream package.json `exports` vs shims.rs registration
// ---------------------------------------------------------------------------

// Mirrors `shims.rs::canonical_virtual_name` / `source_for`.
const REGISTERED_SUBPATHS = {
  "@earendil-works/pi-tui": { ".": "pi_tui_shim.js.txt" },
  "@earendil-works/pi-coding-agent": {
    ".": "pi_coding_agent_shim.js.txt",
    "./rpc-entry": "pi_coding_agent_shim.js.txt",
  },
  "@earendil-works/pi-agent-core": { ".": "pi_agent_core_shim.js.txt" },
  "@earendil-works/pi-ai": {
    ".": "pi_ai_shim.js.txt",
    "./compat": "pi_ai_shim.js.txt",
    "./models": "pi_ai_shim.js.txt",
    "./bedrock-provider": "pi_ai_shim.js.txt",
    "./bun-oauth": "pi_ai_shim.js.txt",
    "./oauth": "pi_ai_oauth_shim.js.txt",
    "./providers/all": "pi_ai_providers_all_shim.js.txt",
  },
  "@earendil-works/pi-codemode": {
    ".": "pi_codemode_shim.js.txt",
    "./declarations": "pi_codemode_shim.js.txt",
    "./source": "pi_codemode_shim.js.txt",
    "./worker": "pi_codemode_shim.js.txt",
    "./portable": "pi_codemode_shim.js.txt",
    "./cloudflare": "pi_codemode_shim.js.txt",
  },
};

function upstreamSubpathList(upstreamDir) {
  const pkgFile = join(UPSTREAM, "packages", upstreamDir, "package.json");
  if (!existsSync(pkgFile)) return [];
  const pkg = JSON.parse(readFileSync(pkgFile, "utf8"));
  const out = [];
  for (const [sub, value] of Object.entries(pkg.exports || {})) {
    if (sub.includes("*") || sub === "./package.json") continue;
    const target =
      typeof value === "string"
        ? value
        : value.source || value.import || value.types || value.default || "";
    const rel = target
      .replace(/^\.\/dist\//, "")
      .replace(/^\.\/src\//, "")
      .replace(/^\.\//, "")
      .replace(/\.d\.ts$/, ".ts")
      .replace(/\.js$/, ".ts");
    out.push({ sub, file: join(UPSTREAM, "packages", upstreamDir, "src", rel) });
  }
  return out;
}

const subpathReport = [];
for (const [pkg, spec] of Object.entries(SHIM_PACKAGES)) {
  if (!spec.upstreamDir) continue;
  const list = upstreamSubpathList(spec.upstreamDir);
  const regs = REGISTERED_SUBPATHS[pkg] || {};
  for (const { sub, file } of list) {
    const short = pkg.replace("@earendil-works/", "");
    const label = `${short}${sub === "." ? "" : sub.slice(1)}`;
    const regShim = regs[sub];
    if (!regShim) {
      subpathReport.push(`  ${label} -> UNREGISTERED in shims.rs (upstream ${file.replace(UPSTREAM, "<pi>")})`);
      continue;
    }
    if (!existsSync(file)) {
      subpathReport.push(`  ${label} -> ${regShim} (upstream file missing: ${file})`);
      continue;
    }
    const upstreamNames = [...analyze(file).map.entries()]
      .filter(([, kind]) => kind === "value")
      .map(([name]) => name);
    const surface = pirSurface(work, pkg + (sub === "." ? "" : sub.slice(1)));
    if (surface.error) {
      subpathReport.push(`  ${label} -> ${regShim} (import failed: ${surface.error})`);
      continue;
    }
    const pirAll = new Set([...surface.named, ...surface.defaultKeys]);
    const missing = upstreamNames.filter((n) => !pirAll.has(n)).sort();
    subpathReport.push(
      `  ${label} -> ${regShim}: upstream=${upstreamNames.length} missing=${missing.length}${missing.length ? " [" + missing.join(", ") + "]" : ""}`,
    );
  }
  for (const sub of Object.keys(regs)) {
    if (!list.some((item) => item.sub === sub)) {
      subpathReport.push(
        `  ${pkg.replace("@earendil-works/", "")}${sub === "." ? "" : sub.slice(1)} -> registered but no upstream exports entry`,
      );
    }
  }
}

const summary =
  `pi SDK surface diff — upstream pi @ ${UPSTREAM} vs pir shims (node ${process.version})\n` +
  `packages audited: ${Object.keys(SHIM_PACKAGES).length}; upstream value names: ${totals.upstream}, ` +
  `pir names: ${totals.pir}, missing: ${totals.missing}\n\n` +
  report.join("\n") +
  "\n\n## upstream value names (parser output, for the regression lock)\n" +
  Object.entries(upstreamLists)
    .map(([key, names]) => `  ${key} (${names.length}): ${names.join(", ")}`)
    .join("\n") +
  `\n\n## subpath entries (upstream package.json exports -> shims.rs)\n` +
  subpathReport.join("\n");

console.log(summary);
const outFile = join(OUT, "pi-surface-diff.txt");
writeFileSync(outFile, summary + "\n", "utf8");
console.log(`\nreport written to ${outFile}`);
