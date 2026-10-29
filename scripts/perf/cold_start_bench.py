#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Small cold-start A/B bench for `pir` (fresh process, wall clock).

Every run is a fresh process (stdin closed, stdout/stderr captured).  Reports
median/min/max over N runs; `--label NAME` prefixes each line so a
before/after pair can be diffed.  Intentionally minimal: reuse this for the
mode-level legs (version/help) and pass `--mock` for print runs against the
in-process mock provider shared with mode_scorecard.py.

Usage:
  python scripts/perf/cold_start_bench.py --label before --runs 5 -- target/debug/pir.exe --version
  python scripts/perf/cold_start_bench.py --label before --runs 3 --print -- target/debug/pir.exe print ...
"""
import argparse
import os
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)


def run_once(cmd, env):
    started = time.perf_counter()
    proc = subprocess.run(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, env=env, timeout=300)
    return (time.perf_counter() - started) * 1000.0, proc.returncode


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--label", default="run")
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--print", dest="do_print", action="store_true",
                    help="serve the shared mock provider and inject its agent dir")
    ap.add_argument("--agent-dir", default="")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    args = ap.parse_args(argv)
    cmd = args.cmd
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    if not cmd:
        print("no command", file=sys.stderr)
        return 2
    env = dict(os.environ)
    env["PIR_NO_UPDATE_CHECK"] = "1"
    env.setdefault("RUST_LOG", "error")
    mock = None
    if args.do_print:
        import big_session_probe as big
        mock = big.Mock(("127.0.0.1", 0), big._Mock)
        import threading
        threading.Thread(target=mock.serve_forever, daemon=True).start()
        port = mock.server_address[1]
        agent = args.agent_dir or big.build_agent_dir(port, 128000)
        env["PIR_AGENT_DIR"] = agent
        env["PI_CODING_AGENT_DIR"] = agent
    elif args.agent_dir:
        env["PIR_AGENT_DIR"] = args.agent_dir
        env["PI_CODING_AGENT_DIR"] = args.agent_dir
    try:
        for _ in range(args.warmup):
            run_once(cmd, env)
        times = []
        exits = []
        for _ in range(args.runs):
            ms, code = run_once(cmd, env)
            times.append(ms)
            exits.append(code)
        print(f"[{args.label}] {' '.join(cmd)}")
        print(f"[{args.label}] n={len(times)} median={statistics.median(times):.1f}ms "
              f"min={min(times):.1f}ms max={max(times):.1f}ms "
              f"samples={[round(t, 1) for t in times]} exits={exits}")
        return 0 if all(c == 0 for c in exits) else 1
    finally:
        if mock is not None:
            mock.shutdown()


if __name__ == "__main__":
    raise SystemExit(main())
