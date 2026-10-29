#!/usr/bin/env node
/**
 * TTFT / prefill curve against a REAL gateway (`WAVE-REPORT.md` §2 evidence,
 * 2026-10-15 follow-up to the "TUI feels slow" report).
 *
 * Runs `pir --mode json --print` with prompts of increasing input size and
 * measures, from the JSON event stream: process start -> first byte, -> the
 * provider request leaving (`message_update/start`), -> first thinking/text
 * delta (TTFT), -> `agent_end`, plus the reported token usage.
 *
 * Hermetic in configuration: an isolated agent dir (models.json +
 * settings.json + auth.json copied, session writes stay there) and stdin
 * closed (`pir print` waits for a non-TTY stdin otherwise).
 *
 * Usage:
 *   node scripts/perf/ttft_curve.mjs --cli target-alt/debug/pir.exe \
 *       [--model openai/gpt-5] [--sizes 1:1.5k,1:10k,1:40k] [--out DIR]
 *   sizes are `<repeat>:<label>`; the prompt appends `<repeat>` filler blocks
 *   of ~1 KB each, so the input grows roughly linearly.
 */
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// ~1 KB per unit, so `<repeat>` approximates the input size in KB
// (about 250-300 tokens per unit for this prose).
const FILLER_UNIT = "The quick brown fox jumps over the lazy dog while the compiler slowly builds the release profile. ";
const FILLER = Array.from({ length: 12 }, () => FILLER_UNIT).join("");

function parseArgs(argv) {
  const args = { cli: null, model: "openai/gpt-5", sizes: null, out: null, timeoutMs: 300000 };
  for (let i = 0; i < argv.length; i += 2) {
    const k = argv[i], v = argv[i + 1];
    if (k === "--cli") args.cli = v;
    else if (k === "--model") args.model = v;
    else if (k === "--sizes") args.sizes = v;
    else if (k === "--out") args.out = v;
    else throw new Error(`unknown arg ${k}`);
  }
  if (!args.cli) throw new Error("--cli is required");
  args.cli = resolve(args.cli);
  if (!args.sizes) args.sizes = "1:1.5k,3:6k,10:20k,20:40k";
  if (!args.out) args.out = join(resolve("logs", "phase2", "perf"), `ttft-${new Date().toISOString().replace(/[-:]/g, "").slice(0, 15)}`);
  return args;
}

function runOnce({ cli, model, repeat, timeoutMs, agentDir, cwd, outDir, label }) {
  const prompt = repeat <= 1
    ? "Reply with exactly: TTFT-OK"
    : `${FILLER.repeat(repeat)}\n\nReply with exactly: TTFT-OK`;
  const started = Date.now();
  // Windows caps a command line at ~32 KB, so anything large goes through
  // stdin (upstream `readPipedStdin` joins it with the positional message) --
  // the same shape a user gets with `cat big | pir print "..."`.
  const useStdin = prompt.length > 7000;
  const argvPrompt = useStdin ? "Reply with exactly: TTFT-OK" : prompt;
  const child = spawn(cli, ["--mode", "json", "--print", "--model", model, argvPrompt], {
    cwd,
    windowsHide: true,
    env: { ...process.env, PIR_AGENT_DIR: agentDir, PI_CODING_AGENT_DIR: agentDir, PIR_NO_UPDATE_CHECK: "1" },
    stdio: [useStdin ? "pipe" : "ignore", "pipe", "pipe"],
  });
  if (useStdin) {
    child.stdin.write(prompt);
    child.stdin.end();
  }
  const events = [];
  let firstByte = null, requestAt = null, ttftAt = null, usage = null, err = "";
  let buffer = "";
  child.stdout.on("data", (chunk) => {
    if (firstByte === null) firstByte = Date.now();
    buffer += chunk.toString("utf8");
    let idx;
    while ((idx = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, idx).trim();
      buffer = buffer.slice(idx + 1);
      if (!line) continue;
      let e = null;
      try { e = JSON.parse(line); } catch { continue; }
      const at = Date.now();
      events.push(e);
      if (e.type === "message_update") {
        const kind = e.assistantMessageEvent?.type;
        if (kind === "start" && requestAt === null) requestAt = at;
        if ((kind === "thinking_delta" || kind === "text_delta") && ttftAt === null) ttftAt = at;
      }
      if (e.type === "message_end" && e.message?.usage) usage = e.message.usage;
    }
  });
  child.stderr.on("data", (chunk) => { err += chunk.toString("utf8"); });
  const done = new Promise((res) => child.on("close", (code) => res(code)));
  return { child, done, get firstByte() { return firstByte; }, get requestAt() { return requestAt; }, get ttftAt() { return ttftAt; }, get usage() { return usage; }, get err() { return err; }, started, events, label, repeat, promptChars: prompt.length, outDir, timeoutMs };
}

function fmt(v) { return v === null || v === undefined ? "—" : `${v}`; }

async function main() {
  const args = parseArgs(process.argv.slice(2));
  mkdirSync(args.out, { recursive: true });
  // Binary identity: recorded so a curve cannot be compared across builds.
  let binary = { path: args.cli, exists: existsSync(args.cli) };
  if (binary.exists) {
    try { binary.sha256 = createHash("sha256").update(readFileSync(args.cli)).digest("hex"); } catch { binary.sha256 = null; }
  }
  const sizes = args.sizes.split(",").map((pair) => {
    const [repeat, label] = pair.split(":");
    return { repeat: Number.parseInt(repeat, 10), label: label ?? `${repeat}x` };
  });
  if (sizes.some((size) => !Number.isInteger(size.repeat) || size.repeat < 1)) {
    throw new Error(`--sizes must be <repeat>:<label> pairs with repeat >= 1, got ${args.sizes}`);
  }

  const rows = [];
  let agentDir = null;
  try {
    // The isolated agent dir is created INSIDE the try so a failure while
    // copying config still removes it (it may hold a credentials copy).
    agentDir = mkdtempSync(join(tmpdir(), "pir-ttft-agent-"));
    const realAgent = join(process.env.USERPROFILE ?? process.env.HOME ?? "", ".pi", "agent");
    for (const name of ["models.json", "settings.json", "auth.json"]) {
      const src = join(realAgent, name);
      if (existsSync(src)) writeFileSync(join(agentDir, name), readFileSync(src));
    }
    const cwd = join(args.out, "work");
    mkdirSync(cwd, { recursive: true });

    for (const size of sizes) {
      const hand = runOnce({
        cli: args.cli, model: args.model, repeat: size.repeat, timeoutMs: args.timeoutMs,
        agentDir, cwd, outDir: args.out, label: size.label,
      });
      const timer = setTimeout(() => {
        if (process.platform === "win32") spawn("taskkill", ["/PID", String(hand.child.pid), "/T", "/F"]);
        else hand.child.kill("SIGKILL");
      }, args.timeoutMs);
      const code = await hand.done;
      clearTimeout(timer);
      const wall = Date.now() - hand.started;
      const row = {
        label: size.label,
        promptChars: hand.promptChars,
        exit: code,
        startupMs: hand.firstByte === null ? null : hand.firstByte - hand.started,
        requestMs: hand.requestAt === null ? null : hand.requestAt - hand.started,
        ttftMs: hand.ttftAt === null ? null : hand.ttftAt - hand.started,
        wallMs: wall,
        tokens: hand.usage ? { input: hand.usage.input, output: hand.usage.output, total: hand.usage.totalTokens } : null,
        error: hand.err.trim().split("\n").slice(0, 2).join(" | "),
      };
      rows.push(row);
      writeFileSync(join(args.out, `events-${size.label}.jsonl`), hand.events.map((e) => JSON.stringify(e)).join("\n"));
      process.stdout.write(`[ttft] ${size.label}: prompt ${row.promptChars} ch -> request ${fmt(row.requestMs)}ms, TTFT ${fmt(row.ttftMs)}ms, wall ${row.wallMs}ms, tokens ${row.tokens ? row.tokens.input : "—"}\n`);
    }
  } finally {
    if (agentDir) rmSync(agentDir, { recursive: true, force: true });
  }

  const lines = [
    "# TTFT / prefill curve (real gateway)",
    "",
    `- CLI: \`${args.cli}\``,
    `- binary: sha256=${binary.sha256 ?? "—"}${binary.exists ? "" : " (NOT FOUND)"}`,
    `- model: \`${args.model}\``,
    `- time: ${new Date().toISOString()}`,
    `- stall rule: a sample with TTFT > 10 000 ms is dirty; compare medians, not single runs`,
    "",
    "| 大小 | prompt 字符 | 启动→请求 | TTFT | 总墙钟 | 输入 tokens | 输出 tokens | 退出 |",
    "|---|---|---|---|---|---|---|---|",
  ];
  for (const r of rows) {
    lines.push(`| ${r.label} | ${r.promptChars} | ${fmt(r.requestMs)}ms | ${fmt(r.ttftMs)}ms | ${r.wallMs}ms | ${r.tokens ? r.tokens.input : "—"} | ${r.tokens ? r.tokens.output : "—"} | ${fmt(r.exit)} |`);
  }
  lines.push("");
  writeFileSync(join(args.out, "report.md"), lines.join("\n"));
  // Machine-readable curve beside the human report.
  writeFileSync(join(args.out, "report.json"), `${JSON.stringify({
    generatedAt: new Date().toISOString(),
    binary,
    model: args.model,
    stallRule: { ttftMs: 10_000 },
    samples: rows.map((r) => ({ ...r, dirty: Number.isFinite(r.ttftMs) && r.ttftMs > 10_000 })),
  }, null, 2)}\n`);
  process.stdout.write(`[ttft] report: ${join(args.out, "report.md")}\n`);
}

main().catch((error) => {
  process.stderr.write(`[ttft] error: ${error.stack ?? error}\n`);
  process.exitCode = 2;
});
