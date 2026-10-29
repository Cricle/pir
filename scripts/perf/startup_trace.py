#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Timestamped stderr/stdout reader for one `pir` child process.

Prints every line with milliseconds since spawn, plus the total wall.  Used to
attribute cold-start phases without rebuilding: run with PI_TIMING=1 (and
PI_STARTUP_BENCHMARK=1 where applicable) and the emitted phase report is
stamped like everything else.

Usage:
  python scripts/perf/startup_trace.py [--mock] [--timeout 120] -- <pir> print ...
"""
import argparse
import os
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--mock", action="store_true")
    ap.add_argument("--stdin-data", default=None, help="send this to stdin, then close")
    ap.add_argument("--timeout", type=float, default=120.0)
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    args = ap.parse_args(argv)
    cmd = args.cmd
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    env = dict(os.environ)
    env["PIR_NO_UPDATE_CHECK"] = "1"
    env.setdefault("RUST_LOG", "error")
    env.setdefault("PI_TIMING", "1")
    mock = None
    if args.mock:
        import big_session_probe as big
        mock = big.Mock(("127.0.0.1", 0), big._Mock)
        threading.Thread(target=mock.serve_forever, daemon=True).start()
        port = mock.server_address[1]
        agent = big.build_agent_dir(port, 128000)
        env["PIR_AGENT_DIR"] = agent
        env["PI_CODING_AGENT_DIR"] = agent

    t0 = time.perf_counter()

    def stamp():
        return (time.perf_counter() - t0) * 1000.0

    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE if args.stdin_data else subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    print(f"{stamp():8.1f} ms | spawn pid={proc.pid}", flush=True)

    def pump(stream, tag):
        for line in iter(stream.readline, b""):
            print(f"{stamp():8.1f} ms | {tag} {line.decode('utf-8', 'replace').rstrip()}",
                  flush=True)
        stream.close()

    t_err = threading.Thread(target=pump, args=(proc.stderr, "ERR"), daemon=True)
    t_out = threading.Thread(target=pump, args=(proc.stdout, "OUT"), daemon=True)
    t_err.start()
    t_out.start()
    if args.stdin_data:
        proc.stdin.write(args.stdin_data.encode())
        proc.stdin.close()
    try:
        proc.wait(timeout=args.timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    t_err.join(timeout=5)
    t_out.join(timeout=5)
    print(f"{stamp():8.1f} ms | exit {proc.returncode}", flush=True)
    if mock is not None:
        mock.shutdown()
    return proc.returncode


if __name__ == "__main__":
    raise SystemExit(main())
