#!/usr/bin/env node
/**
 * Re-score an existing harder-tasks campaign from its archived work dirs.
 *
 * The first campaign had a verdict-parser bug (pretty-printed JSON); the work
 * dirs were archived, so the scores can be rebuilt offline without re-running
 * either CLI.
 *
 * Usage: node scripts/perf/harder_rescore.mjs [--out logs/perf/harder-1]
 */
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const TASKS_DIR = join(HERE, "tasks");

function parseArgs(argv) {
  const a = { out: join(REPO, "logs", "perf", "harder-1") };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--out") a.out = argv[++i];
  }
  return a;
}

function verdict(task, dir) {
  const r = spawnSync(process.execPath, [join(TASKS_DIR, task, "hidden", "verdict.mjs"), dir], {
    encoding: "utf8", windowsHide: true, timeout: 300_000,
  });
  const out = `${r.stdout ?? ""}`.trim();
  const start = out.indexOf("{");
  if (start >= 0) {
    try { return JSON.parse(out.slice(start)); } catch {}
  }
  return { score: null, passed: null, total: null, notes: [`parse failed (exit ${r.status}): ${out.slice(0, 200)}`] };
}

const a = parseArgs(process.argv.slice(2));
const outDir = resolve(a.out);
const reportPath = join(outDir, "report.json");
const rows = JSON.parse(readFileSync(reportPath, "utf8"));
for (const row of rows) {
  const dir = join(outDir, `${row.task}-${row.label}-r${row.rep}`);
  if (!existsSync(dir)) { console.error(`missing work dir for ${row.task}/${row.label}: ${dir}`); continue; }
  const v = verdict(row.task, dir);
  row.score = v.score; row.passed = v.passed; row.total = v.total; row.notes = v.notes ?? [];
  console.log(`${row.task.padEnd(16)} ${row.label} → score ${row.score} (${row.passed}/${row.total})`);
}
writeFileSync(reportPath, JSON.stringify(rows, null, 2));
const md = ["# Harder offline tasks (pir vs pi) — hidden-test quality scores", "", "| task | CLI | score | passed/total | wall | TTFT | turns | tools | tokens | timeout |", "|---|---|---|---|---|---|---|---|---|---|"];
for (const r of rows) {
  md.push(`| ${r.task} | ${r.label} | ${r.score} | ${r.passed}/${r.total} | ${Math.round(r.wallMs / 1000)}s | ${r.ttftMs ? Math.round(r.ttftMs) + "ms" : "—"} | ${r.turns} | ${r.calls} | ${r.tokens} | ${r.timedOut ? "YES" : ""} |`);
}
writeFileSync(join(outDir, "report.md"), md.join("\n") + "\n");
const totals = {};
for (const r of rows) { (totals[r.label] ??= { s: 0, n: 0, passed: 0, total: 0 }); totals[r.label].s += r.score ?? 0; totals[r.label].n++; totals[r.label].passed += Number.isFinite(r.passed) ? r.passed : 0; totals[r.label].total += Number.isFinite(r.total) ? r.total : 0; }
for (const [k, t] of Object.entries(totals)) console.log(`${k}: mean score ${(t.s / t.n).toFixed(3)} over ${t.n} tasks (${t.passed}/${t.total} checks)`);

// Keep the machine-readable summary in sync with the rescored rows: only the
// score block changes (timing medians are untouched by an offline rescore).
const summaryPath = join(outDir, "summary.json");
if (existsSync(summaryPath)) {
  try {
    const summary = JSON.parse(readFileSync(summaryPath, "utf8"));
    for (const [label, t] of Object.entries(totals)) {
      if (!summary.legs?.[label]) continue;
      const mine = rows.filter((r) => r.label === label);
      summary.legs[label].score = {
        mean: Number((t.s / t.n).toFixed(4)),
        passedChecks: t.passed,
        totalChecks: t.total,
        scoredRuns: mine.filter((r) => Number.isFinite(r.score)).length,
        unscoredRuns: mine.filter((r) => !Number.isFinite(r.score)).length,
        fullMarks: mine.filter((r) => Number.isFinite(r.total) && r.score === r.total).length,
      };
    }
    summary.rescoredAt = new Date().toISOString();
    writeFileSync(summaryPath, JSON.stringify(summary, null, 2));
  } catch (error) {
    console.error(`could not refresh ${summaryPath}: ${error?.message ?? error}`);
  }
}
