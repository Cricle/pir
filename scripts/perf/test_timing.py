#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Per-test wall attribution for an already-built libtest binary.

Runs the binary SERIALLY (`--test-threads=1`) and timestamps every line libtest
prints on stdout. libtest emits `test NAME ... ok` when a test *finishes*, so on
a serial run the gap between two result lines is that test's wall time (the
first line's start is the process spawn).

Read-only: it never builds anything, it only executes a binary the caller
names. Test binaries are the normal `target*/debug/deps/<suite>-<hash>.exe`.

Usage:
  python scripts/perf/test_timing.py target/debug/deps/g_agent-<hash>.exe
  python scripts/perf/test_timing.py <exe> --top 25 --json logs/lane-speed-rss/t.json
"""

import argparse
import json
import os
import subprocess
import sys
import time


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("exe")
    ap.add_argument("--top", type=int, default=20)
    ap.add_argument("--json", default="")
    ap.add_argument("--filter", default="", help="substring filter passed as libtest positional")
    ap.add_argument("--threads", type=int, default=1)
    args = ap.parse_args()

    cmd = [os.path.abspath(args.exe), "--test-threads", str(args.threads), "--nocapture"]
    if args.filter:
        cmd.append(args.filter)

    t0 = time.perf_counter()
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
        errors="replace",
    )
    results = []  # (seconds_since_prev_result, test_name, status)
    last = t0
    total_lines = 0
    assert proc.stdout is not None
    for line in proc.stdout:
        now = time.perf_counter()
        total_lines += 1
        stripped = line.rstrip("\r\n")
        if stripped.startswith("test ") and (" ... ok" in stripped or " ... FAILED" in stripped
                                             or " ... ignored" in stripped):
            name = stripped[len("test "):].split(" ... ", 1)[0]
            status = stripped.rsplit(" ... ", 1)[1].split()[0]
            results.append((now - last, name, status))
            last = now
        elif stripped.startswith("test result:"):
            pass
    rc = proc.wait()
    wall = time.perf_counter() - t0

    results.sort(key=lambda r: r[0], reverse=True)
    print(f"exe: {args.exe}")
    print(f"process wall {wall:.3f}s, {len(results)} tests, rc={rc}")
    print(f"{'sec':>8}  status   test")
    for sec, name, status in results[: args.top]:
        print(f"{sec:8.3f}  {status:<8} {name}")
    accounted = sum(r[0] for r in results)
    print(f"accounted {accounted:.3f}s of {wall:.3f}s ({100.0 * accounted / wall:.1f}%)")

    if args.json:
        os.makedirs(os.path.dirname(os.path.abspath(args.json)) or ".", exist_ok=True)
        with open(args.json, "w", encoding="utf-8") as fh:
            json.dump(
                {
                    "exe": os.path.abspath(args.exe),
                    "wall": wall,
                    "rc": rc,
                    "tests": [
                        {"sec": s, "name": n, "status": st} for s, n, st in results
                    ],
                },
                fh,
                indent=1,
            )
    return rc


if __name__ == "__main__":
    sys.exit(main())
