#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Run the TUI once under ConPTY with PI_TIMING=1, wait for the first frame,
send /quit, and dump the PTY transcript (which carries the TUI phase report).

Usage:
  python scripts/perf/tui_phase_trace.py [--pir target/debug/pir.exe] [--timeout 60]
"""
import argparse
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(REPO, "scripts", "tui_conpty"))

import tui_drive  # noqa: E402

FRAME_END = b"\x1b[?2026l"


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--pir", default=os.path.join(REPO, "target", "debug", "pir.exe"))
    ap.add_argument("--timeout", type=float, default=60.0)
    ap.add_argument("--agent-dir", default="")
    args = ap.parse_args(argv)

    agent = args.agent_dir or os.path.join(REPO, "logs", "lane-cold-start", "tmp-agent")
    os.makedirs(agent, exist_ok=True)
    work = os.path.join(REPO, "logs", "lane-cold-start", "tmp-work")
    os.makedirs(work, exist_ok=True)
    cmdline = f'"{os.path.abspath(args.pir)}" interactive --no-session'
    env = dict(tui_drive.BASE_ENV)
    env.update({"PIR_AGENT_DIR": agent, "PI_CODING_AGENT_DIR": agent,
                "PIR_NO_UPDATE_CHECK": "1", "RUST_LOG": "error", "PI_TIMING": "1"})
    started = time.perf_counter()
    pty = tui_drive.Pty(cmdline, work, env)
    firstbyte = None
    frame = None
    deadline = started + args.timeout
    while time.perf_counter() < deadline:
        data = bytes(pty.snapshot())
        if firstbyte is None and data:
            firstbyte = (time.perf_counter() - started) * 1000
        if FRAME_END in data:
            frame = (time.perf_counter() - started) * 1000
            break
        if not pty.running():
            break
        time.sleep(0.01)
    print(f"first byte {firstbyte} ms, first frame {frame} ms")
    time.sleep(0.3)
    pty.write(b"/quit\r")
    if not pty.wait(8):
        pty.write(b"\x03")
        pty.wait(3)
    data = bytes(pty.snapshot())
    text = data.decode("utf-8", "replace")
    for line in text.splitlines():
        if "timing" in line.lower() or "Startup" in line or "ms" in line and line.strip().startswith(
                ("parseArgs", "runMigrations", "loadSettings", "startupSetup", "tui.", "TOTAL")):
            print(repr(line))
    try:
        pty.terminate()
        pty.close()
    except Exception:
        pass
    log = os.path.join(agent, "pir.log")
    if os.path.exists(log):
        print("--- pir.log tail ---")
        with open(log, encoding="utf-8", errors="replace") as fh:
            print(fh.read()[-4000:])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
