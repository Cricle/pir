#!/usr/bin/env node
/**
 * Offline per-turn timeline decomposition for the ab-bird A/B event archives.
 *
 * Reads `logs/perf/ab-bird/events-{pir,pi}-r{1,2}.jsonl` and splits each leg's
 * wall clock into four observable buckets per turn:
 *
 *   provider : turn_start -> first streaming delta   (request build + upload +
 *              gateway queue + prefill; first-byte latency)
 *   stream   : first delta -> last delta             (token streaming span)
 *   tail     : last delta -> assistant message_end   (final chunk / usage)
 *   tools    : tool_execution_start -> end spans     (durations reported by CLI)
 *   idle     : everything else (turn bookkeeping, next request construction,
 *              event-queue drain, scheduler)
 *
 * Also prints per-turn usage (input/cacheRead/output/reasoning), delta mix, and
 * every inter-event gap above a threshold (stall hunting), so gateway queueing
 * can be separated from CLI-side stalls.
 *
 * Usage:
 *   node scripts/perf/ab_timeline.mjs [--dir logs/perf/ab-bird] [--gap-ms 3000]
 *        [--csv out.csv] [--md out.md] [--json out.json]
 */

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");

function parseArgs(argv) {
  const args = { dir: join(REPO, "logs", "perf", "ab-bird"), gapMs: 3000, legs: ["pir-r1", "pi-r1", "pir-r2", "pi-r2"], csv: null, md: null, json: null };
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    const next = () => { i += 1; if (i >= argv.length) throw new Error(`${flag} needs a value`); return argv[i]; };
    switch (flag) {
      case "--dir": args.dir = next(); break;
      case "--gap-ms": args.gapMs = Number.parseInt(next(), 10); break;
      case "--legs": args.legs = next().split(","); break;
      case "--csv": args.csv = next(); break;
      case "--md": args.md = next(); break;
      case "--json": args.json = next(); break;
      default: throw new Error(`unknown flag ${flag}`);
    }
  }
  return args;
}

// ab_bird compacts message_update to `assistantEvent`; older archives kept
// the full `assistantMessageEvent` object. Read both so either generation
// produces non-zero stream/delta numbers.
const eventKind = (e) => e.assistantEvent ?? e.assistantMessageEvent?.type ?? null;
const isDelta = (e) => e.type === "message_update" && /_delta$/.test(eventKind(e) ?? "");

function loadLeg(path) {
  const events = readFileSync(path, "utf8").split("\n").filter(Boolean).map((line) => JSON.parse(line));
  const turns = [];
  let cur = null;
  for (const e of events) {
    if (e.type === "session" || e.type === "agent_start") continue;
    if (e.type === "turn_start") {
      cur = {
        start: e._at, events: [e], deltas: [], tools: [], msgEnd: null, turnEnd: null,
        firstDelta: null, lastDelta: null, lastEvent: e._at, firstAny: null,
        usage: null, deltaMix: {}, firstLineAt: null,
      };
      turns.push(cur);
      continue;
    }
    if (!cur) continue;
    cur.events.push(e);
    cur.lastEvent = Math.max(cur.lastEvent, e._at);
    if (e.type === "message_update") {
      if (cur.firstAny === null) cur.firstAny = e._at;
      if (isDelta(e)) {
        cur.deltas.push(e);
        const kind = eventKind(e);
        cur.deltaMix[kind] = (cur.deltaMix[kind] ?? 0) + 1;
        if (cur.firstDelta === null) cur.firstDelta = e._at;
        cur.lastDelta = e._at;
      }
    }
    if (e.type === "message_end" && e.message?.role === "assistant") { cur.msgEnd = e._at; cur.usage = e.message.usage ?? null; cur.stopReason = e.message.stopReason ?? null; }
    if (e.type === "tool_execution_start" || e.type === "tool_execution_end") cur.tools.push(e);
    if (e.type === "turn_end") cur.turnEnd = e._at;
    if (e.type === "agent_end") cur.agentEnd = e._at;
    if (e.type === "agent_settled") cur.agentSettled = e._at;
  }
  return { events, turns };
}

function legSummary(leg, turns, gapMs) {
  const bucket = { provider: 0, stream: 0, tail: 0, tools: 0, idle: 0 };
  const rows = [];
  for (let i = 0; i < turns.length; i += 1) {
    const t = turns[i];
    const next = turns[i + 1];
    const end = (next ? next.start : (t.agentSettled ?? t.agentEnd ?? t.lastEvent)) ?? t.lastEvent;
    const provider = t.firstDelta === null ? (t.msgEnd ?? end) - t.start : t.firstDelta - t.start;
    const streamRaw = t.firstDelta === null ? 0 : (t.lastDelta - t.firstDelta);
    // `streamRaw` spans tool starts/ends when a turn makes several calls; the
    // tool bucket below adds those durations separately. Subtract the tool
    // duration from the stream bucket to avoid double-counting it.
    const toolDur = t.tools.filter((e) => e.type === "tool_execution_end").reduce((s, e) => s + (e.durationMs ?? 0), 0);
    const stream = Math.max(0, streamRaw - toolDur);
    const tail = t.lastDelta === null ? 0 : ((t.msgEnd ?? end) - t.lastDelta);
    const covered = provider + stream + tail + toolDur;
    const total = end - t.start;
    const idle = Math.max(0, total - covered);
    bucket.provider += provider; bucket.stream += stream; bucket.tail += tail;
    bucket.tools += toolDur; bucket.idle += idle;
    const u = t.usage ?? {};
    const errTools = t.tools.filter((e) => e.type === "tool_execution_end" && e.isError).length;
    rows.push({
      turn: i + 1, start: t.start, end, total,
      provider, stream, streamRaw, tail, toolDur, idle,
      inTok: u.input ?? 0, cacheRead: u.cacheRead ?? 0, out: u.output ?? 0,
      reasoning: u.reasoning ?? 0,
      reasoningReported: Boolean(t.usage && Object.prototype.hasOwnProperty.call(t.usage, "reasoning")),
      totalTokens: u.totalTokens ?? 0,
      nTools: t.tools.filter((e) => e.type === "tool_execution_end").length,
      errTools, stopReason: t.stopReason ?? "",
      deltas: t.deltas.length, deltaMix: t.deltaMix,
    });
  }
  const wall = rows.length ? (rows[rows.length - 1].end - rows[0].start) : 0;
  const sum = bucket.provider + bucket.stream + bucket.tail + bucket.tools + bucket.idle;
  return { rows, bucket, wall, sum, leg, gapMs };
}

// --- stall scan: inter-event gaps over the whole leg -----------------------
function gapScan(events, gapMs) {
  const out = [];
  for (let i = 1; i < events.length; i += 1) {
    const gap = events[i]._at - events[i - 1]._at;
    if (gap >= gapMs) {
      out.push({ at: events[i - 1]._at, gap, from: describe(events[i - 1]), to: describe(events[i]) });
    }
  }
  return out;
}

/**
 * Classify each inter-event gap >= gapMs into the phase it falls in, so the
 * stall budget can be attributed: a gap whose two ends are both streaming
 * deltas is a mid-stream provider stall; a gap that starts at the turn's
 * `start`/message_start and ends at the first delta is a first-byte (queue or
 * retry-backoff) stall; a gap inside a tool_execution_start/end pair is tool
 * time (already counted separately, listed for contrast).
 */
function classifyGaps(events, gapMs) {
  const perTurn = new Map();
  let cur = null;
  const push = (e) => {
    if (e.type === "turn_start") {
      cur = { start: e._at, deltas: [], openTools: new Map(), tools: [], msgEnd: null };
      perTurn.set(cur, cur);
    }
    if (!cur) return;
    if (isDelta(e)) cur.deltas.push(e);
    if (e.type === "tool_execution_start") cur.openTools.set(e.toolCallId, e._at);
    if (e.type === "tool_execution_end") {
      const start = cur.openTools.get(e.toolCallId);
      if (start != null) cur.tools.push({ start, end: e._at, tool: e.toolName });
      cur.openTools.delete(e.toolCallId);
    }
  };
  for (const e of events) push(e);
  // Classification: the gap's END relative to the turn's FIRST delta decides.
  // A gap that ends at (or before) the first delta is a first-byte / queue /
  // retry-backoff stall; a gap that starts at (or after) the first delta is a
  // mid-stream stall (the connection is open and deltas were flowing).
  const phaseOf = (a, b) => {
    let found = null;
    // Strict `>`: a turn boundary shares its timestamp with the previous
    // turn's last event (the next turn_start is emitted right at the end of
    // the previous turn), so `>=` would misattribute that boundary gap to the
    // turn that has not streamed anything yet.
    for (const t of perTurn.values()) if (b > t.start) found = t;
    if (!found) return "other";
    if (!found.deltas.length) return "wait";
    const first = found.deltas[0]._at;
    if (b <= first + 50) return "wait";
    if (a >= first - 50) return "stream";
    return "other";
  };
  const toolIntervals = [];
  for (const turn of perTurn.values()) {
    for (const tool of turn.tools) toolIntervals.push(tool);
  }
  const buckets = { wait: [], stream: [], tool: [], other: [] };
  for (let i = 1; i < events.length; i += 1) {
    const gap = events[i]._at - events[i - 1]._at;
    if (gap < gapMs) continue;
    const a = events[i - 1]._at;
    const b = events[i]._at;
    const inTool = toolIntervals.some((tool) => a >= tool.start && b <= tool.end);
    const phase = inTool ? "tool" : phaseOf(a, b);
    (buckets[phase] ?? buckets.other).push({ at: a, gap, from: describe(events[i - 1]), to: describe(events[i]) });
  }
  return buckets;
}

function describe(e) {
  if (e.type === "message_update") {
    const kind = eventKind(e);
    return `message_update:${kind ?? "?"}`;
  }
  if (e.type === "tool_execution_start" || e.type === "tool_execution_end") {
    return `${e.type}:${e.toolName}${e.isError ? "(err)" : ""}${e.durationMs != null ? `(${e.durationMs}ms)` : ""}`;
  }
  if (e.type === "message_start" && e.message?.role) return `message_start:${e.message.role}`;
  if (e.type === "message_end" && e.message?.role) return `message_end:${e.message.role}`;
  return e.type;
}

const pct = (a, b) => (b === 0 ? "0.0" : ((a / b) * 100).toFixed(1));
const s = (ms) => (ms / 1000).toFixed(1);

function main() {
  const args = parseArgs(process.argv.slice(2));
  const csvRows = [["leg", "turn", "start_ms", "end_ms", "total_ms", "provider_ms", "stream_ms", "tail_ms", "tool_ms", "idle_ms", "in_tok", "cache_read", "out_tok", "reasoning_tok", "reasoning_reported", "total_tok", "tools", "tool_errors", "deltas", "stop_reason"].join(",")];

  const summaries = [];
  for (const leg of args.legs) {
    const path = join(args.dir, `events-${leg}.jsonl`);
    if (!existsSync(path)) { process.stderr.write(`missing ${path}\n`); continue; }
    const { events, turns } = loadLeg(path);
    const summary = legSummary(leg, turns, args.gapMs);
    summaries.push(summary);
    for (const r of summary.rows) {
      csvRows.push([leg, r.turn, r.start, r.end, r.total, Math.round(r.provider), Math.round(r.stream), Math.round(r.tail), Math.round(r.toolDur), Math.round(r.idle),
        r.inTok, r.cacheRead, r.out, r.reasoning, r.reasoningReported ? 1 : 0, r.totalTokens, r.nTools, r.errTools, r.deltas, r.stopReason].join(","));
    }
  }

  // ---- printed report ----
  const out = [];
  out.push("# ab-bird timeline decomposition (offline, from events-*.jsonl)");
  out.push("");
  for (const S of summaries) {
    const b = S.bucket;
    out.push(`## ${S.leg}`);
    out.push("");
    out.push(`turns=${S.rows.length}  wall(sum of turns)=${(S.wall / 1000).toFixed(1)}s`);
    out.push("");
    out.push("| bucket | ms | %wall |");
    out.push("|---|---|---|");
    out.push(`| provider (turn_start→1st delta) | ${Math.round(b.provider)} | ${pct(b.provider, S.wall)}% |`);
    out.push(`| stream (1st delta→last delta) | ${Math.round(b.stream)} | ${pct(b.stream, S.wall)}% |`);
    out.push(`| tail (last delta→message_end) | ${Math.round(b.tail)} | ${pct(b.tail, S.wall)}% |`);
    out.push(`| tools | ${Math.round(b.tools)} | ${pct(b.tools, S.wall)}% |`);
    out.push(`| idle (bookkeeping/scheduler) | ${Math.round(b.idle)} | ${pct(b.idle, S.wall)}% |`);
    out.push("");
    out.push("| turn | t_start(s) | total(s) | provider(s) | stream(s) | tail(s) | tool(s) | idle(s) | in/cacheRead/out | reason | tools(err) | deltas | stop |");
    out.push("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for (const r of S.rows) {
      out.push(`| ${r.turn} | ${s(r.start)} | ${s(r.total)} | ${s(r.provider)} | ${s(r.stream)} | ${s(r.tail)} | ${s(r.toolDur)} | ${s(r.idle)} | ${r.inTok}/${r.cacheRead}/${r.out} | ${r.reasoning} | ${r.nTools}(${r.errTools}) | ${r.deltas} | ${r.stopReason} |`);
    }
    const { events: legEvents } = loadLeg(join(args.dir, `events-${S.leg}.jsonl`));
    const buckets = classifyGaps(legEvents, args.gapMs);
    const sumOf = (list) => list.reduce((a, b) => a + b.gap, 0);
    const stallWait = sumOf(buckets.wait);
    const stallStream = sumOf(buckets.stream);
    const stallTool = sumOf(buckets.tool);
    const stallOther = sumOf(buckets.other);
    const stallAll = stallWait + stallStream + stallTool + stallOther;
    out.push("");
    out.push(`stalls >= ${args.gapMs} ms  (${buckets.wait.length + buckets.stream.length + buckets.tool.length + buckets.other.length} gaps, ${s(stallAll)}s = ${pct(stallAll, S.wall)}% of turn wall)`);
    out.push(`- first-byte/queue stalls : ${s(stallWait)}s over ${buckets.wait.length} gaps`);
    out.push(`- mid-stream stalls       : ${s(stallStream)}s over ${buckets.stream.length} gaps`);
    out.push(`- tool start→end gaps     : ${s(stallTool)}s over ${buckets.tool.length} gaps`);
    out.push(`- other gaps (pre-tool etc): ${s(stallOther)}s over ${buckets.other.length} gaps`);
    out.push(`- wall without stalls     : ${s(Math.max(0, S.wall - stallAll))}s`);
    for (const g of [...buckets.wait, ...buckets.stream, ...buckets.tool, ...buckets.other]) {
      out.push(`- t=${s(g.at)}s gap=${s(g.gap)}s  ${g.from}  ==>  ${g.to}`);
    }
    out.push("");
  }

  const text = out.join("\n");
  process.stdout.write(text + "\n");
  if (args.md) writeFileSync(args.md, `${text}\n`);
  if (args.csv) writeFileSync(args.csv, `${csvRows.join("\n")}\n`);
  if (args.json) {
    // Machine-readable per-leg decomposition + stall buckets, so a campaign
    // summary can cite the same numbers as the human report.
    const payload = {
      dir: args.dir.replace(/\\/g, "/"),
      gapMs: args.gapMs,
      legs: summaries.map((S) => ({
        leg: S.leg,
        turns: S.rows.length,
        wallMs: S.wall,
        buckets: S.bucket,
        gapBuckets: (() => {
          const buckets = classifyGaps(loadLeg(join(args.dir, `events-${S.leg}.jsonl`)).events, args.gapMs);
          const sum = (list) => list.reduce((a, b) => a + b.gap, 0);
          return {
            waitMs: sum(buckets.wait), waitGaps: buckets.wait.length,
            streamMs: sum(buckets.stream), streamGaps: buckets.stream.length,
            toolMs: sum(buckets.tool), toolGaps: buckets.tool.length,
            otherMs: sum(buckets.other), otherGaps: buckets.other.length,
          };
        })(),
        turnsDetail: S.rows,
      })),
    };
    writeFileSync(args.json, `${JSON.stringify(payload, null, 2)}\n`);
  }
  // Keep the env fallback for existing automation.
  if (process.env.AB_TIMELINE_CSV) writeFileSync(process.env.AB_TIMELINE_CSV, csvRows.join("\n"));
}

main();
