# -*- coding: utf-8 -*-
"""Size AND speed comparison for two `pir` builds (the slimming gate).

The user's constraint for the size work is "functionality must not change" plus
"performance must not degrade" — so a binary-slimming change is only acceptable
with both numbers side by side:

  * size: the file size of each binary;
  * cold start: the median wall time of `--version` over N runs (this machine's
    process-spawn floor is ~1.0-1.1 s, measured with `node --version`);
  * one real turn: `scripts/perf/big_session_probe.py --size-mb 1` against the
    mock provider, which reports wall time, peak RSS, request count and the
    largest request body — the same instrument the memory work used, so a
    regression in either direction is visible.

Usage:
  python scripts/perf/size_compare.py --baseline logs/perf/size/pir-release-thin-lto-cgu16.exe \
                                      --candidate target-alt/release/pir.exe
"""

import argparse
import os
import re
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)


def size_mb(path):
    return os.path.getsize(path) / (1024 * 1024)


def startup_median_ms(exe, runs):
    times = []
    for _ in range(runs):
        started = time.perf_counter()
        subprocess.run([exe, "--version"], capture_output=True, check=False)
        times.append((time.perf_counter() - started) * 1000.0)
    return statistics.median(times), min(times)


def turn_metrics(exe, size_mb_session, out_dir):
    """One prompt against the local mock provider via the big-session probe."""
    env = dict(os.environ)
    env["PYTHONIOENCODING"] = "utf-8"
    result = subprocess.run(
        [
            sys.executable,
            os.path.join(HERE, "big_session_probe.py"),
            "--only", "pir",
            "--pir", exe,
            "--size-mb", str(size_mb_session),
            # ONE SESSION FILE PER LEG: sharing it would let the second leg
            # inherit the first one's appended entries and compaction
            # checkpoint, so the two legs would do different work (observed:
            # 2 vs 4 requests, which made the RSS/wall comparison meaningless).
            "--session-name",
            f"size-cmp-{os.path.basename(exe)}-{size_mb_session}mb.jsonl",
            "--out", out_dir,
        ],
        capture_output=True,
        text=True,
        env=env,
        cwd=REPO,
    )
    wall = None
    rss = None
    requests = None
    largest = None
    for line in (result.stdout or "").splitlines():
        m = re.search(r"wall=([0-9.]+)s peakRSS=([0-9.]+) MB .*requests=(\d+) "
                      r"largest_request=(\d+)", line)
        if m:
            wall, rss, requests, largest = (
                float(m.group(1)), float(m.group(2)), int(m.group(3)), int(m.group(4)),
            )
    return {"wallS": wall, "peakRssMb": rss, "requests": requests,
            "largestRequestBytes": largest, "rc": result.returncode}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--baseline", required=True, help="the pre-change release binary")
    ap.add_argument("--candidate", required=True, help="the slimmed release binary")
    ap.add_argument("--runs", type=int, default=10, help="startup samples per binary")
    ap.add_argument("--turn-size-mb", type=int, default=1)
    ap.add_argument("--skip-turn", action="store_true",
                    help="size + startup only (the turn probes take ~1 min each)")
    ap.add_argument("--out", default=os.path.join(REPO, "logs", "perf", "size"))
    args = ap.parse_args(argv)

    for path in (args.baseline, args.candidate):
        if not os.path.exists(path):
            print(f"ERROR: missing binary {path}", file=sys.stderr)
            return 2
    os.makedirs(args.out, exist_ok=True)

    rows = []
    for label, exe in (("baseline", args.baseline), ("candidate", args.candidate)):
        median, best = startup_median_ms(exe, args.runs)
        row = {"label": label, "path": exe, "sizeMb": round(size_mb(exe), 1),
               "startupMedianMs": round(median, 1), "startupBestMs": round(best, 1)}
        if not args.skip_turn:
            row.update(turn_metrics(exe, args.turn_size_mb,
                                    os.path.join(args.out, label)))
        rows.append(row)

    base, cand = rows[0], rows[1]
    ds = cand["sizeMb"] - base["sizeMb"]
    print(f"{'build':10} {'size MiB':>9} {'startup med ms':>15} {'turn wall s':>12} "
          f"{'turn peak MB':>13} {'requests':>9}")
    for row in rows:
        print(f"{row['label']:10} {row['sizeMb']:>9.1f} {row['startupMedianMs']:>15.1f} "
              f"{str(row.get('wallS')):>12} {str(row.get('peakRssMb')):>13} "
              f"{str(row.get('requests')):>9}")
    print(f"\nsize: {base['sizeMb']:.1f} -> {cand['sizeMb']:.1f} MiB "
          f"({ds:+.1f} MiB, {(ds / base['sizeMb'] * 100):+.1f}%)")
    if not args.skip_turn and base.get("wallS") and cand.get("wallS"):
        dw = cand["wallS"] - base["wallS"]
        print(f"turn wall: {base['wallS']:.2f} -> {cand['wallS']:.2f} s ({dw:+.2f} s)")
        dr = (cand["peakRssMb"] or 0) - (base["peakRssMb"] or 0)
        print(f"turn peak RSS: {base['peakRssMb']} -> {cand['peakRssMb']} MB ({dr:+.1f} MB)")
    verdict = "OK" if ds < 0 else "NO SIZE WIN"
    print(f"verdict: {verdict}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
