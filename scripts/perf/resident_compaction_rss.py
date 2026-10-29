#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Peak-RSS probe for a RESIDENT compaction on a large session (RPC mode).

Why: `--print` opens a paged session and compacts the bounded tail window, so
it does not exercise the resident path. The RPC/TUI/SDK hosts build an
`AgentSession`, which widens to the full transcript; a prompt on a session that
crosses the compaction threshold then runs the resident pipeline and appends a
compaction entry. `append_compaction` used to materialize a second full
projection (one clone of every message) just to replay the system prompt into
that entry. This probe drives the real RPC host, samples the process tree's
working set, and reports the peak of one prompt that compacts.

Read-only with respect to the repo: it copies the session file into a temp
work dir and writes only under --out (plus a temp agent dir).

Usage:
  python scripts/perf/resident_compaction_rss.py \
      --pir target/debug/pir.exe \
      --session logs/lane-speed-rss/big-session-50mb/work/big.jsonl \
      --context-window 32000 --keep-recent 4000 --reserve 2000 \
      --out logs/lane-speed-rss/resident-compaction-after

Prints one JSON line with peak RSS, wall, compaction frames and the binary
identity, and writes <out>/summary.json.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(REPO, "scripts", "soak"))

import big_session_probe as big  # noqa: E402
import fleet_simulation as fleet  # noqa: E402
import soak_common as common  # noqa: E402


class Sampler(threading.Thread):
    def __init__(self, pid, interval=0.05):
        super().__init__(daemon=True)
        self.pid = pid
        self.interval = interval
        self.peak = 0.0
        self.samples = []
        self._stop_event = threading.Event()

    def run(self):
        while not self._stop_event.is_set():
            value = big.sample_rss_mb(self.pid)
            if value is not None:
                self.peak = max(self.peak, value)
                self.samples.append([round(time.time(), 3), round(value, 1)])
            self._stop_event.wait(self.interval)

    def stop(self):
        self._stop_event.set()
        self.join(timeout=5)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--pir", default=os.path.join(REPO, "target", "debug", "pir.exe"))
    ap.add_argument("--session", required=True)
    ap.add_argument("--context-window", type=int, default=32000)
    ap.add_argument("--reserve", type=int, default=2000)
    ap.add_argument("--keep-recent", type=int, default=4000)
    ap.add_argument("--mode", choices=("rpc", "print"), default="rpc")
    ap.add_argument("--timeout", type=float, default=300.0)
    ap.add_argument("--out", default=os.path.join(REPO, "logs", "lane-speed-rss", "resident-compaction"))
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    work = tempfile.mkdtemp(prefix="pir-resident-compaction-")
    agent_dir = os.path.join(work, "agent")
    session_copy = os.path.join(work, "big.jsonl")
    shutil.copyfile(args.session, session_copy)

    mock = common.MockProvider()
    mock.start()
    try:
        common.provision_mock_agent(
            agent_dir,
            mock,
            args.context_window,
            reserve_tokens=args.reserve,
            keep_recent_tokens=args.keep_recent,
        )
        env = common.build_child_env(agent_dir)
        if args.mode == "rpc":
            argv = [
                os.path.abspath(args.pir),
                "--mode", "rpc",
                "--session", session_copy,
                "--model", "mock/soak-model",
                "--thinking", "off",
                "--no-context-files",
            ]
        else:
            argv = [
                os.path.abspath(args.pir),
                "--mode", "json",
                "--print",
                "--session", session_copy,
                "--model", "mock/soak-model",
                "--thinking", "off",
                "--no-context-files",
                "go",
            ]
        started = time.time()
        proc = subprocess.Popen(
            argv,
            cwd=work,
            env=env,
            stdin=subprocess.PIPE if args.mode == "rpc" else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            errors="replace",
        )
        handle = fleet.open_process(proc.pid)
        sampler = Sampler(proc.pid)
        sampler.start()

        def send(obj):
            proc.stdin.write(json.dumps(obj) + "\n")
            proc.stdin.flush()

        deadline = started + args.timeout
        events = {}
        prompt_seen = args.mode == "print"
        agent_end = False
        if args.mode == "rpc":
            send({"type": "hello"})
            send({"id": "p1", "type": "prompt", "text": "go"})
        while time.time() < deadline:
            line = proc.stdout.readline()
            if not line:
                break
            try:
                value = json.loads(line)
            except ValueError:
                continue
            kind = value.get("type")
            events[kind] = events.get(kind, 0) + 1
            if value.get("command") == "prompt":
                prompt_seen = True
            if kind == "agent_end":
                agent_end = True
                break
        wall = time.time() - started
        try:
            if proc.stdin:
                proc.stdin.close()
        except Exception:
            pass
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            common.kill_process_tree(proc.pid)
            proc.wait(timeout=10)
        sampler.stop()
        cpu_seconds = fleet.proc_cpu_s(handle) if handle else None
        stderr = proc.stderr.read()
        summary = {
            "binary": big.binary_identity(args.pir),
            "sessionBytes": os.path.getsize(session_copy),
            "contextWindow": args.context_window,
            "keepRecentTokens": args.keep_recent,
            "reserveTokens": args.reserve,
            "wallSeconds": round(wall, 2),
            "peakRssMb": round(sampler.peak, 1),
            "cpuSeconds": round(cpu_seconds, 3) if cpu_seconds else None,
            "samples": len(sampler.samples),
            "promptSeen": prompt_seen,
            "agentEnd": agent_end,
            "events": events,
            "exit": proc.returncode,
            "stderrTail": stderr[-2000:],
        }
        with open(os.path.join(args.out, "summary.json"), "w", encoding="utf-8") as fh:
            json.dump(summary, fh, indent=1)
        print(json.dumps({k: summary[k] for k in (
            "sessionBytes", "contextWindow", "wallSeconds", "peakRssMb",
            "promptSeen", "agentEnd", "exit")}, indent=1))
        return 0 if summary["agentEnd"] else 3
    finally:
        mock.stop()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
