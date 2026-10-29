#!/usr/bin/env node
/**
 * Merge the four ab-bird legs onto one absolute wall-clock axis and classify
 * every request's first-byte wait / streaming span, so gateway-side stalls can
 * be told apart from CLI-side stalls: the legs ran pir,pi,pir,pi back-to-back,
 * so a stall that shows up in the OTHER leg's neighbourhood is gateway load.
 *
 * Usage: node scripts/perf/ab_absolute.mjs [--dir logs/perf/ab-bird] [--dir2 DIR]
 *        [--slow-ms 10000]
 */
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");

function parseArgs(argv) {
  const args = { dir: join(REPO, "logs", "perf", "ab-bird"), dir2: join(REPO, "logs", "perf", "ab-probe"), slowMs: 10000 };
  for (let i = 0; i < argv.length; i += 1) {
    const f = argv[i];
    const next = () => { i += 1; if (i >= argv.length) throw new Error(`${f} needs a value`); return argv[i]; };
    if (f === "--dir") args.dir = next();
    else if (f === "--dir2") args.dir2 = next();
    else if (f === "--slow-ms") args.slowMs = Number.parseInt(next(), 10);
    else throw new Error(`unknown ${f}`);
  }
  return args;
}

function loadLeg(path) {
  const evs = readFileSync(path, "utf8").split("\n").filter(Boolean).flatMap((line) => {
    try { return [JSON.parse(line)]; } catch { return []; }
  });
  if (evs.length === 0) return { t0: null, turns: [] };
  // The session header normally carries both fields. Old/partial archives may
  // start with an event without a timestamp; use the first event that has both.
  const stamp = evs.find((e) => typeof e.timestamp === "string"
    && Number.isFinite(Date.parse(e.timestamp))
    && Number.isFinite(e._at));
  const t0 = stamp ? Date.parse(stamp.timestamp) - stamp._at : null;
  const turns = [];
  let cur = null;
  for (const e of evs) {
    if (e.type === "turn_start") { cur = { start: e._at, firstDelta: null, lastDelta: null, msgEnd: null, tools: 0, toolErr: 0, usage: null, deltas: 0 }; turns.push(cur); continue; }
    if (!cur) continue;
    if (e.type === "message_update") {
      // Compact ab_bird archives use `assistantEvent`; older ones kept the
      // full `assistantMessageEvent` object.
      const kind = e.assistantEvent ?? e.assistantMessageEvent?.type ?? "";
      if (/_delta$/.test(kind)) {
        if (cur.firstDelta === null) cur.firstDelta = e._at;
        cur.lastDelta = e._at; cur.deltas += 1;
      }
    }
    if (e.type === "message_end" && e.message?.role === "assistant") { cur.msgEnd = e._at; cur.usage = e.message.usage ?? null; }
    if (e.type === "tool_execution_end") { cur.tools += 1; if (e.isError) cur.toolErr += 1; }
  }
  return { t0, turns };
}

const args = parseArgs(process.argv.slice(2));
const rows = [];
for (const dir of [args.dir, args.dir2]) {
  if (!existsSync(dir)) continue;
  for (const file of readdirSync(dir)) {
    const m = /^events-(?<leg>.+)-r(?<run>\d+)\.jsonl$/.exec(file);
    if (!m) continue;
    const { t0, turns } = loadLeg(join(dir, file));
    for (const [i, t] of turns.entries()) {
      const wait = t.firstDelta === null ? null : t.firstDelta - t.start;
      const stream = t.firstDelta === null ? null : t.lastDelta - t.firstDelta;
      const u = t.usage ?? {};
      rows.push({
        leg: `${m.groups.leg}-r${m.groups.run}`, turn: i + 1,
        absStart: t0 === null ? null : t0 + t.start, wait, stream,
        inTok: u.input ?? 0, cr: u.cacheRead ?? 0, out: u.output ?? 0, reason: u.reasoning ?? 0,
        tools: t.tools, toolErr: t.toolErr, deltas: t.deltas,
      });
    }
  }
}
rows.sort((a, b) => (a.absStart ?? Infinity) - (b.absStart ?? Infinity));
const iso = (ms) => (ms === null ? "—" : new Date(ms).toISOString().slice(11, 23));
const s = (v) => (v === null ? "—" : (v / 1000).toFixed(1));

const legs = [...new Set(rows.map((r) => r.leg))];
const stats = (sel) => {
  const ws = sel.filter((r) => r.wait !== null).map((r) => r.wait);
  const ss = sel.filter((r) => r.stream !== null).map((r) => r.stream);
  const sum = (a) => a.reduce((x, y) => x + y, 0);
  const q = (a, p) => { const b = [...a].sort((x, y) => x - y); return b.length ? b[Math.min(b.length - 1, Math.floor(p * b.length))] : 0; };
  return {
    n: sel.length, waitSum: sum(ws), waitMed: q(ws, 0.5), waitP90: q(ws, 0.9), waitMax: Math.max(0, ...ws),
    waitSlow: ws.filter((w) => w >= args.slowMs).length,
    streamSum: sum(ss), streamMed: q(ss, 0.5), streamMax: Math.max(0, ...ss),
    streamSlow: ss.filter((w) => w >= args.slowMs).length,
  };
};

console.log("### per-leg request latency stats");
console.log(`slow threshold = ${args.slowMs} ms`);
console.log("| leg | reqs | wait sum(s) | wait med(s) | wait p90(s) | wait max(s) | waits>=slow | stream sum(s) | stream med(s) | stream max(s) | streams>=slow |");
console.log("|---|---|---|---|---|---|---|---|---|---|---|");
for (const leg of legs) {
  const st = stats(rows.filter((r) => r.leg === leg));
  console.log(`| ${leg} | ${st.n} | ${s(st.waitSum)} | ${s(st.waitMed)} | ${s(st.waitP90)} | ${s(st.waitMax)} | ${st.waitSlow} | ${s(st.streamSum)} | ${s(st.streamMed)} | ${s(st.streamMax)} | ${st.streamSlow} |`);
}

console.log("\n### absolute timeline (all legs merged), slow requests flagged");
console.log("| abs time | leg | turn | wait(s) | stream(s) | in/cr | out | reason | tools(err) |");
console.log("|---|---|---|---|---|---|---|---|---|");
for (const r of rows) {
  const flag = (r.wait !== null && r.wait >= args.slowMs) || (r.stream !== null && r.stream >= args.slowMs) ? " **" : "";
  console.log(`| ${iso(r.absStart)}${flag} | ${r.leg} | ${r.turn} | ${s(r.wait)} | ${s(r.stream)} | ${r.inTok}/${r.cr} | ${r.out} | ${r.reason} | ${r.tools}(${r.toolErr}) |`);
}

// storms: cluster slow starts in absolute time
console.log("\n### slow-request clusters (absolute, any leg)");
const slowRuns = [];
for (const r of rows) {
  const isSlow = (r.wait !== null && r.wait >= args.slowMs) || (r.stream !== null && r.stream >= args.slowMs);
  if (!isSlow) continue;
  if (r.absStart === null) continue;
  const last = slowRuns[slowRuns.length - 1];
  if (last && r.absStart - last.end <= 180000) { last.end = r.absStart; last.items.push(`${r.leg}#${r.turn}(w${s(r.wait)}/s${s(r.stream)})`); }
  else slowRuns.push({ start: r.absStart, end: r.absStart, items: [`${r.leg}#${r.turn}(w${s(r.wait)}/s${s(r.stream)})`] });
}
for (const run of slowRuns) {
  console.log(`- ${iso(run.start)} → ${iso(run.end)}  ${run.items.join(", ")}`);
}
