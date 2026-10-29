#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Wall-time A/B for the slow test binaries (read-only: runs existing exes).

The runner matches `scripts/test_fast.ps1`'s inner shape: each binary is
executed directly with `--test-threads=N` (default 4, the script's default).
No build and no cargo invocation: pass the paths the foreman's build produced.

Usage:
  python scripts/perf/measure_slow_targets.py \
      --deps target/debug/deps \
      --targets g_agent,g_cli_server,brush_embed,mcp_integration,fs_watch_incremental,shell_command_timeout \
      --reps 2 --out logs/lane-speed-rss/wall-after.json

It resolves `<target>-<16hex>.exe` to the NEWEST match per target (the same
rule test_fast uses), so a rebuilt binary is picked up automatically. Skips
targets with no binary instead of failing the whole run.
"""

import argparse
import glob
import json
import os
import re
import subprocess
import sys
import time

EXE_RE = re.compile(r"-[0-9a-f]{16}\.exe$")


def newest_exe(deps: str, target: str):
    hits = [p for p in glob.glob(os.path.join(deps, target + "-*.exe")) if EXE_RE.search(p)]
    if not hits:
        return None
    return max(hits, key=os.path.getmtime)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--deps", default=os.path.join("target", "debug", "deps"))
    ap.add_argument("--targets", required=True, help="comma-separated cargo target names")
    ap.add_argument("--reps", type=int, default=2)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    targets = [t.strip() for t in args.targets.split(",") if t.strip()]
    rows = []
    for target in targets:
        exe = newest_exe(args.deps, target)
        if not exe:
            print(f"{target:24} MISSING (no {target}-<hash>.exe in {args.deps})")
            rows.append({"target": target, "missing": True})
            continue
        walls = []
        rc = None
        for _ in range(max(1, args.reps)):
            started = time.perf_counter()
            done = subprocess.run(
                [exe, f"--test-threads={args.threads}"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                stdin=subprocess.DEVNULL,
            )
            walls.append(round(time.perf_counter() - started, 3))
            rc = done.returncode
        rows.append(
            {
                "target": target,
                "exe": os.path.abspath(exe),
                "mtime": time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(os.path.getmtime(exe))),
                "threads": args.threads,
                "walls": walls,
                "best": min(walls),
                "rc": rc,
            }
        )
        print(f"{target:24} best={min(walls):7.2f}s  runs={walls}  rc={rc}")

    if args.out:
        os.makedirs(os.path.dirname(os.path.abspath(args.out)) or ".", exist_ok=True)
        with open(args.out, "w", encoding="utf-8") as fh:
            json.dump({"rows": rows}, fh, indent=1)
    return 0


if __name__ == "__main__":
    sys.exit(main())
