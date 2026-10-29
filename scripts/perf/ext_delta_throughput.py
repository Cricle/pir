#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Per-delta extension dispatch cost: reasoning/content delta stream, headless.

Why this exists (2026-10-16): the user's "interactive mode is extremely slow and
eats huge memory" report was root-caused to the per-delta `message_update`
extension dispatch, not the TUI: with loose numbers on a 10 000-delta stream a
single no-op handler took `pir print` from ~3 s / ~40 MB to ~25 s / ~442 MB
because the hook payload serialized the whole accumulated message twice per
delta (plus a JS round trip). This harness reproduces that measurement hermetically
(local mock stream, no gateway, no TUI) so the fix can be verified and regressions
caught later.

Usage:
    python scripts/perf/ext_delta_throughput.py --pir target-alt/release/pir.exe
    python scripts/perf/ext_delta_throughput.py --reasoning 8000 --content 2000

Legs: `none` (no extension), `noop` (a `pi.on("message_update", () => {})`
handler), `stringify` (handler that also serializes `event.message`). Each leg
runs the same stream against a fresh isolated agent dir; wall clock and peak
working set are reported per leg.

Measurement contract (why the sampler looks the way it does):
  * RSS is the WHOLE PROCESS TREE (CIM `Win32_Process`), sampled every
    `--tree-interval` seconds (default 0.1 s, first sample immediately). A
    10 000-delta stream finishes in a few seconds, so the old 3 s cadence
    could land ZERO samples in the window where the per-delta allocation
    spikes — the one moment this harness exists to see. Each CIM query costs
    ~0.7 s on this host, so the effective cadence is that floor, stated here
    rather than implied.
  * The `message_update` backlog bound (`MESSAGE_UPDATE_BACKLOG_BOUND = 16`
    in `src/agent/agent_loop.rs`) is INTERNAL: the binary exposes no counter
    for it. This harness therefore cannot assert "the queue stayed <= 16"; it
    asserts the OBSERVABLE consequence — a `noop` handler's per-delta cost must
    stay near the `none` leg's (wall and peak), which is only possible while
    the dispatcher's queue is bounded and the wire is O(delta). A regression
    to O(reply)-per-delta shows up as wall/peak growth, not as a queue depth.
    `--max-noop-ratio` (off unless passed) turns that into an exit code; see
    MEMORY-PROBE-NOTES.md.
"""

import argparse
import hashlib
import http.server
import json
import os
import re
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)

# Shared instruments (binary identity + staleness note) live in the big-session
# probe; importing them keeps every perf probe reporting the same evidence
# fields instead of four private variants.
import big_session_probe as big  # noqa: E402

STREAM_PLAN = {"reasoning": 8000, "content": 2000, "delay_ms": 0.0, "tool_args": 0}

# `tool-args` leg: one tool call whose JSON arguments arrive as <=32-char
# fragments (the mock builds a ~24*tool_args byte argument string). The
# `ToolCallDelta` wire ships the provider's authoritative *parsed* block per
# delta — O(args) per delta, O(args²) per call — which the text-only legs
# cannot see (found by the round-2 read-only review).
TOOL_ARGS_FRAGMENT = 32


def _sse(payload: dict) -> bytes:
    return ("data: " + json.dumps(payload) + "\n\n").encode("utf-8")


def _chunk(completion_id: str, created: int, model: str, delta: dict, finish=None,
           usage=None) -> dict:
    payload = {
        "id": completion_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }
    if usage is not None:
        payload["usage"] = usage
    return payload


class _Handler(http.server.BaseHTTPRequestHandler):
    plan = dict(STREAM_PLAN)
    requests = 0

    def log_message(self, fmt, *args):  # silence stderr chatter
        pass

    def do_GET(self):  # /v1/models
        body = json.dumps({"object": "list", "data": [{"id": "mock-model", "object": "model"}]})
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body.encode("utf-8"))

    def do_POST(self):  # /v1/chat/completions
        length = int(self.headers.get("Content-Length") or 0)
        request = json.loads(self.rfile.read(length) or b"{}")
        model = request.get("model") or "mock-model"
        type(self).requests += 1
        reasoning = int(self.plan["reasoning"])
        content = int(self.plan["content"])
        tool_args = int(self.plan.get("tool_args") or 0)
        delay = float(self.plan["delay_ms"]) / 1000.0
        completion_id = f"mock-{type(self).requests}"
        created = int(time.time())

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        try:
            if type(self).requests == 1 and tool_args:
                # One tool call, arguments fragmented across many deltas.
                self.wfile.write(_sse(_chunk(completion_id, created, model,
                                             {"role": "assistant", "content": ""})))
                arguments = json.dumps({"blob": "x" * (24 * tool_args)})
                first = True
                for offset in range(0, len(arguments), TOOL_ARGS_FRAGMENT):
                    call = {"index": 0,
                            "function": {"arguments": arguments[offset:offset + TOOL_ARGS_FRAGMENT]}}
                    if first:
                        call["id"] = "call_probe"
                        call["type"] = "function"
                        call["function"]["name"] = "probe_args"
                        first = False
                    self.wfile.write(_sse(_chunk(completion_id, created, model,
                                                 {"tool_calls": [call]})))
                    if delay:
                        time.sleep(delay)
                self.wfile.write(_sse(_chunk(completion_id, created, model,
                                             {}, finish="tool_calls")))
                self.wfile.write(b"data: [DONE]\n\n")
                return
            self.wfile.write(_sse(_chunk(completion_id, created, model,
                                         {"role": "assistant", "content": ""})))
            for i in range(reasoning):
                self.wfile.write(_sse(_chunk(
                    completion_id, created, model,
                    {"reasoning_content": f"thought {i:05d} "})))
                if delay:
                    time.sleep(delay)
            for i in range(content):
                self.wfile.write(_sse(_chunk(
                    completion_id, created, model, {"content": f"word{i:05d} "})))
                if delay:
                    time.sleep(delay)
            self.wfile.write(_sse(_chunk(
                completion_id, created, model, {}, finish="stop",
                usage={"prompt_tokens": 100, "completion_tokens": reasoning + content,
                       "total_tokens": 100 + reasoning + content})))
            self.wfile.write(b"data: [DONE]\n\n")
        except (BrokenPipeError, ConnectionResetError):
            return


class _Server(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


EXTENSIONS = {
    "none": None,
    # Extension loaded and active, but NO handler registered: isolates "the
    # extension runtime exists" from "a message_update handler dispatches".
    "loaded": (
        "export default (pi) => {\n"
        "  globalThis.__loaded = true;\n"
        "};\n"
    ),
    "noop": (
        "export default (pi) => {\n"
        "  pi.on(\"message_update\", () => {});\n"
        "};\n"
    ),
    # The real-world shape (pi-phone): scalar reads on the cumulative message
    # plus forwarding the per-delta event. Must cost O(delta), not O(reply).
    "read-role": (
        "export default (pi) => {\n"
        "  let last = null;\n"
        "  pi.on(\"message_update\", (event) => {\n"
        "    if (event.message?.role === \"assistant\") last = event.message.timestamp;\n"
        "    globalThis.__last = event.assistantMessageEvent?.type ?? last;\n"
        "  });\n"
        "};\n"
    ),
    # Reads the accumulated content array every delta (no stringify): the
    # heaviest legitimate observer pattern.
    "read-content": (
        "export default (pi) => {\n"
        "  let seen = 0;\n"
        "  pi.on(\"message_update\", (event) => {\n"
        "    const c = event.message?.content;\n"
        "    if (Array.isArray(c)) seen = c.length;\n"
        "    globalThis.__seen = seen;\n"
        "  });\n"
        "};\n"
    ),
    "stringify": (
        "export default (pi) => {\n"
        "  pi.on(\"message_update\", (event) => { globalThis.__last = "
        "JSON.stringify(event.message).length; });\n"
        "};\n"
    ),
    # One tool call with thousands of small argument fragments (the mock's
    # `tool_args` plan): the per-delta `ToolCallDelta` wire carries the
    # provider's authoritative parsed block, so this leg isolates the
    # O(args²)-per-call wire cost the text legs cannot see.
    "tool-args": (
        "export default (pi) => {\n"
        "  pi.on(\"message_update\", () => {});\n"
        "};\n"
    ),
}


def _sha256(path: str):
    """SHA-256 of a local file, or None when unreadable (identity record only)."""
    try:
        digest = hashlib.sha256()
        with open(path, "rb") as fh:
            for chunk in iter(lambda: fh.read(1 << 20), b""):
                digest.update(chunk)
        return digest.hexdigest()
    except OSError:
        return None


def build_agent_dir(root: str, port: int, leg: str) -> str:
    agent = os.path.join(root, "agent")
    os.makedirs(agent, exist_ok=True)
    models = {"providers": {"mock": {
        "name": "Mock",
        "baseUrl": f"http://127.0.0.1:{port}/v1",
        "api": "openai-completions",
        "apiKey": "sk-mock-delta-throughput",
        "models": [{
            "id": "mock-model",
            "name": "Mock",
            "reasoning": True,
            "input": ["text"],
            "contextWindow": 128000,
            "maxTokens": 8192,
        }],
    }}}
    with open(os.path.join(agent, "models.json"), "w", encoding="utf-8") as fh:
        json.dump(models, fh, indent=2)
    settings = {"defaultProvider": "mock", "defaultModel": "mock-model"}
    with open(os.path.join(agent, "settings.json"), "w", encoding="utf-8") as fh:
        json.dump(settings, fh, indent=2)
    source = EXTENSIONS[leg]
    if source is not None:
        ext_dir = os.path.join(agent, "extensions")
        os.makedirs(ext_dir, exist_ok=True)
        with open(os.path.join(ext_dir, "delta_probe.ts"), "w", encoding="utf-8") as fh:
            fh.write(source)
    return agent


def _sample_rss_mb(pid: int):
    """Peak working set in MB via tasklist (Windows); None when unavailable."""
    try:
        done = subprocess.run(
            ["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
            capture_output=True, text=True, timeout=10,
        )
        out = done.stdout
    except Exception:
        return None
    if not out:
        return None
    match = re.search(r'"([\d.,]+) K"', out)
    if not match:
        return None
    return float(match.group(1).replace(",", "").replace(".", "")) / 1024.0


def _tree_rss_mb(root_pid: int):
    """
    Working set of the WHOLE process tree rooted at `root_pid`, in MB.

    `tasklist` reports one pid, and on Windows the CLI is often reached through
    a shim, so a single-pid sample measures the shim rather than the agent.
    PowerShell CIM exposes ParentProcessId, so the tree sum is the fair number;
    when it is unavailable the caller keeps the single-pid tasklist peak.
    """
    script = (
        "Get-CimInstance Win32_Process | ForEach-Object { "
        "\"$($_.ProcessId) $($_.ParentProcessId) $($_.WorkingSetSize)\" }"
    )
    try:
        done = subprocess.run(
            ["powershell", "-NoProfile", "-Command", script],
            capture_output=True, text=True, timeout=20,
        )
    except Exception:
        return None
    rows = []
    for line in (done.stdout or "").splitlines():
        parts = line.split()
        if len(parts) != 3:
            continue
        try:
            rows.append(tuple(int(part) for part in parts))
        except ValueError:
            continue
    if not rows:
        return None
    children, own = {}, {}
    for pid, ppid, ws in rows:
        own[pid] = ws
        children.setdefault(ppid, []).append(pid)
    total = own.get(root_pid, 0)
    stack = [root_pid]
    while stack:
        for kid in children.get(stack.pop(), []):
            total += own.get(kid, 0)
            stack.append(kid)
    return total / (1024.0 * 1024.0)


class _TreeSampler(threading.Thread):
    """Background process-tree RSS sampler: never blocks the leg's drain loop.

    Interval contract: the sampler takes its FIRST sample immediately, then
    one per `interval`. A delta leg can finish in ~2 s, and a sampler that
    sleeps before its first query reports `peak_rss_mb = 0.0` for a leg it
    never looked at (the tool-args report showed 4 samples for a 1.6 s leg,
    i.e. it caught nothing but the tail). `samples == 0` is reported as an
    unmeasured leg, never as 0 MB.
    """

    def __init__(self, pid: int, interval: float = 0.1):
        super().__init__(daemon=True)
        self.pid = pid
        self.interval = interval
        self.peak_mb = 0.0
        self.samples = 0
        self.peak_at_s = None
        self.started = time.time()
        self._stop = threading.Event()
        self._first = True

    def run(self):
        while self._first or not self._stop.wait(self.interval):
            self._first = False
            value = _tree_rss_mb(self.pid)
            if value is not None:
                self.samples += 1
                if value > self.peak_mb:
                    self.peak_mb = value
                    self.peak_at_s = round(time.time() - self.started, 2)
            if self._stop.is_set():
                break

    def stop(self):
        self._stop.set()


def run_leg(pir: str, root: str, port: int, leg: str, prompt: str, timeout: float,
            tree_interval: float = 0.1):
    agent = build_agent_dir(root, port, leg)
    env = dict(os.environ)
    env["PIR_AGENT_DIR"] = agent
    started = time.time()
    # Streams the reply to stdout as it arrives: drain to FILES, never to an
    # unread pipe (a chatty undrained pipe wedges the child — same rule the
    # test suites document).
    out_path = os.path.join(root, f"{leg}.stdout.txt")
    err_path = os.path.join(root, f"{leg}.stderr.txt")
    with open(out_path, "wb") as out_fh, open(err_path, "wb") as err_fh:
        proc = subprocess.Popen(
            [pir, "print", "--no-session", "--model", "mock/mock-model", prompt],
            cwd=agent, env=env,
            stdin=subprocess.DEVNULL,  # print reads a non-TTY stdin as piped input; keep it closed
            stdout=out_fh, stderr=err_fh,
        )
    peak = 0.0
    samples = 0
    peak_at_s = None
    sample_enabled = os.environ.get("EXT_DELTA_NO_RSS") != "1"
    sampler = _TreeSampler(proc.pid, tree_interval) if sample_enabled and os.name == "nt" else None
    if sampler is not None:
        sampler.start()
    while proc.poll() is None:
        if time.time() - started > timeout:
            proc.kill()
            break
        if sampler is None and sample_enabled:
            rss = _sample_rss_mb(proc.pid)
            if rss is not None:
                peak = max(peak, rss)
                samples += 1
        time.sleep(0.25)
    if sampler is not None:
        sampler.stop()
        sampler.join(timeout=5)
        peak = sampler.peak_mb
        samples = sampler.samples
        peak_at_s = sampler.peak_at_s
    proc.wait(timeout=30)
    wall = time.time() - started
    with open(out_path, "r", encoding="utf-8", errors="replace") as fh:
        out = fh.read()
    with open(err_path, "r", encoding="utf-8", errors="replace") as fh:
        err = fh.read()
    # A completed leg answers with the streamed text; a leg that only produced
    # reasoning answers with the "no text" shape, which is fine for a cost probe.
    return {
        "leg": leg,
        "wall_s": round(wall, 2),
        # Contract: None means "no sample landed", never 0.0 (a leg that exited
        # before the first CIM query returned must not rank as a zero-cost leg).
        "peak_rss_mb": round(peak, 1) if samples else None,
        "peak_at_s": peak_at_s,
        "samples": samples,
        "exit_code": proc.returncode,
        "stdout_chars": len(out or ""),
        "stderr_tail": (err or "").strip().splitlines()[-2:],
    }


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pir", default=os.path.join(REPO, "target-alt", "release", "pir.exe"))
    ap.add_argument("--reasoning", type=int, default=STREAM_PLAN["reasoning"])
    ap.add_argument("--content", type=int, default=STREAM_PLAN["content"])
    ap.add_argument("--delay-ms", type=float, default=STREAM_PLAN["delay_ms"],
                    help="per-delta delay (0 = as fast as the mock can write)")
    ap.add_argument("--legs", default="none,noop,stringify")
    ap.add_argument("--tool-args", type=int, default=3000,
                    help="argument fragments for the `tool-args` leg (~24*N bytes of JSON)")
    ap.add_argument("--out", default=os.path.join(REPO, "logs", "perf"))
    ap.add_argument("--timeout", type=float, default=300.0)
    ap.add_argument("--tree-interval", type=float, default=0.1,
                    help="process-tree RSS sampling interval in seconds (the first sample is "
                         "taken immediately; each CIM query costs ~0.7 s on this host, so "
                         "this is a floor, not a promise)")
    ap.add_argument("--max-noop-ratio", type=float, default=None,
                    help="exit 1 when the noop leg's wall (or peak RSS) exceeds this multiple "
                         "of the none leg's: the observable form of the bounded "
                         "message_update backlog. Omit to report only.")
    args = ap.parse_args(argv)

    if not os.path.isfile(args.pir):
        print(f"ERROR: pir binary not found: {args.pir}", file=sys.stderr)
        return 2
    binary = {"path": os.path.abspath(args.pir), "size": os.path.getsize(args.pir),
              "mtime": time.strftime("%Y-%m-%dT%H:%M:%S",
                                     time.localtime(os.path.getmtime(args.pir))),
              "sha256": _sha256(args.pir), "version": None}
    note = big.staleness_note(args.pir)
    if note and note["stale"]:
        print(f"[ext-delta] STALE BINARY: built {note['binaryMtime']}, but "
              f"{note['newestSource']} is newer ({note['newestSourceMtime']}) — "
              f"SMOKE TEST, not a measurement; rebuild before recording the numbers")
    _Handler.plan = {"reasoning": args.reasoning, "content": args.content,
                     "delay_ms": args.delay_ms, "tool_args": 0}
    server = _Server(("127.0.0.1", 0), _Handler)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    stamp = time.strftime("%Y%m%d-%H%M%S")
    root = os.path.join(args.out, f"ext-delta-{stamp}")
    os.makedirs(root, exist_ok=True)
    print(f"[ext-delta] pir={args.pir}")
    print(f"[ext-delta] stream: {args.reasoning} reasoning + {args.content} content deltas, "
          f"delay={args.delay_ms} ms/ delta; out={root}")
    rows = []
    for leg in [l for l in args.legs.split(",") if l]:
        # Per-leg plan: the tool-args leg streams one fragmented tool call and
        # answers the follow-up request with a single content delta; every
        # other leg gets the plain reasoning/content stream.
        _Handler.requests = 0
        if leg == "tool-args":
            _Handler.plan = {"reasoning": 0, "content": 1, "delay_ms": args.delay_ms,
                             "tool_args": args.tool_args}
        else:
            _Handler.plan = {"reasoning": args.reasoning, "content": args.content,
                             "delay_ms": args.delay_ms, "tool_args": 0}
        row = run_leg(args.pir, os.path.join(root, leg), port, leg, "say something",
                      args.timeout, args.tree_interval)
        rows.append(row)
        peak_text = ("  unmeasured" if row["peak_rss_mb"] is None
                     else f"{row['peak_rss_mb']:7.1f} MB")
        print(f"[ext-delta] {leg:10s} wall={row['wall_s']:8.2f}s "
              f"peak_rss={peak_text} samples={row['samples']} exit={row['exit_code']} "
              f"stdout={row['stdout_chars']} chars")
        if row["stderr_tail"]:
            print(f"            stderr: {row['stderr_tail']}")
        # Contract: a leg that never ran to completion (non-zero exit, or a
        # timeout kill) has no comparable cost, so the comparison below is
        # skipped rather than judged on a partial stream.
        if row["exit_code"] != 0:
            print(f"            INVALID: exit={row['exit_code']} (timeout={args.timeout}s "
                  f"or a harness failure); this leg's wall/peak are not comparable")
    # A prior version/call must not leak into this report.
    try:
        version = subprocess.run([os.path.abspath(args.pir), "--version"], capture_output=True,
                                 text=True, timeout=15)
        binary["version"] = (version.stdout or "").strip().splitlines()[0] if version.returncode == 0 else None
    except Exception:
        binary["version"] = None
    server.shutdown()
    report = {"stamp": stamp, "pir": args.pir, "binary": binary, "plan": _Handler.plan, "rows": rows}
    with open(os.path.join(root, "report.json"), "w", encoding="utf-8") as fh:
        json.dump(report, fh, indent=2)
    print(f"[ext-delta] report: {os.path.join(root, 'report.json')}")
    # Contract: the bounded `message_update` backlog is not externally
    # observable, so the gate is its observable consequence. `none` vs `noop`
    # run the identical stream; if dispatching a no-op handler per delta stays
    # O(delta) behind a bounded queue, wall and peak must stay near the
    # extension-free leg. `--max-noop-ratio` is opt-in so the default run still
    # reports numbers on a machine too noisy for a ratio gate.
    by_leg = {row["leg"]: row for row in rows}
    if args.max_noop_ratio is not None:
        none_row, noop_row = by_leg.get("none"), by_leg.get("noop")
        if not none_row or not noop_row or none_row["exit_code"] or noop_row["exit_code"]:
            print(f"[ext-delta] GATE INVALID: needs clean `none` and `noop` legs "
                  f"(got none={bool(none_row)} noop={bool(noop_row)})")
            return 3
        wall_ratio = noop_row["wall_s"] / none_row["wall_s"] if none_row["wall_s"] else 0.0
        rss_ratio = 0.0
        if none_row["peak_rss_mb"] and noop_row["peak_rss_mb"]:
            rss_ratio = noop_row["peak_rss_mb"] / none_row["peak_rss_mb"]
        verdict = "WITHIN" if max(wall_ratio, rss_ratio) <= args.max_noop_ratio else "OVER"
        print(f"[ext-delta] GATE noop/none: wall x{wall_ratio:.2f}, peak x{rss_ratio:.2f} "
              f"({verdict} the x{args.max_noop_ratio:.2f} allowance)")
        if verdict == "OVER":
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
