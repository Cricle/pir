#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Peak-RSS probe for one `pir` run (headless), with a real agent dir.

Why: the interactive process the user watches sits at 144-170 MB while a task
runs; this probe measures the *floor* of the same binary + the same extension
packages on a one-delta task, so the resident size can be attributed instead of
guessed.

Instrument contract: the measured number is the peak WORKING SET of the whole
PROCESS TREE (root + descendants), sampled through CIM (`Win32_Process`
ParentProcessId), because the agent spawns tool/extension children that a
single-pid `tasklist` sample cannot see. The cheap single-pid `tasklist` value
is still sampled as a fallback and is reported as `scope: root` — never
relabelled as a tree number. On a non-Windows host the probe reports `scope:
root` (POSIX `ps` is not wired here); read that label before quoting a number.

Usage:
    python scripts/perf/rss_probe.py --pir target-alt/release/pir.exe \
        --model openai/gpt-5 --thinking off "say ok"

Exit code: 0 measured, 1 no sample landed or the child failed, 2 binary missing.
`--agent-dir` left unset means the process default (`~/.pi/agent`): that is the
real extension pack the attribution needs, and it is READ by the child; the
probe never writes into it (`print --no-session`). GATE-RUNBOOK §4 forbids
pointing a *session-writing* harness at `~/.pi` — this leg writes no session,
but the runbook text and this default still disagree; see
scripts/perf/MEMORY-PROBE-NOTES.md.

stdout/stderr are redirected to files under the run dir (streaming the reply to
an unread pipe wedges the child), stdin is DEVNULL.
"""

import argparse
import os
import re
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)

# The tree instrument is shared with big_session_probe (Toolhelp32 walk with a
# CIM fallback) so `rss_probe`'s number and the big/TUI probes' numbers are the
# same quantity measured the same way.
import big_session_probe as big  # noqa: E402

CIM_SCRIPT = ("Get-CimInstance Win32_Process | ForEach-Object { "
              '"$($_.ProcessId) $($_.ParentProcessId) $($_.WorkingSetSize)" }')


def sample_root_rss_mb(pid: int):
    """Working set (MB) of ONE pid via `tasklist` (~50 ms). Fallback only."""
    try:
        done = subprocess.run(
            ["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
            capture_output=True, text=True, errors="replace", timeout=10,
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


# The tree query is the shared implementation. `sample_tree_rss_mb` keeps its
# old name/signature so existing call sites read the same.
def sample_tree_rss_mb(root_pid: int, timeout: float = 15.0):
    """Working set (MB) of the WHOLE process tree rooted at `root_pid`.

    Contract: the tree total is the attribution measurement; a root-only peak
    is reported separately as `scope: root` so a shim/child cost can never be
    hidden inside a number labelled "tree". Fast Toolhelp32 walk first, CIM
    fallback (both from `big_session_probe`, so the quantity is identical).
    """
    return big.sample_rss_mb(root_pid, timeout)


class TreeSampler(threading.Thread):
    """Background tree sampler: keeps sampling off the main poll loop.

    Interval contract: `interval` is the spacing between samples (default
    0.25 s) and the FIRST query runs immediately. The tree query itself is
    `big_session_probe.sample_rss_mb`: a Toolhelp32 walk (milliseconds) with the
    CIM/PowerShell query as fallback, so the cadence is real on this host
    instead of being floored at ~1.5 s by CIM. `samples == 0` means the leg was
    never measured and is reported as an invalid run, never as a 0 MB floor.

    The stop flag is `_stop_event`, not `_stop`: `threading.Thread` already has
    a private `_stop()` method, and shadowing it made `join()` raise
    `TypeError: 'Event' object is not callable` after the leg finished.
    """

    def __init__(self, pid: int, interval: float):
        super().__init__(daemon=True)
        self.pid = pid
        self.interval = interval
        self.peak = 0.0
        self.samples = 0
        self.series = []
        self._stop_event = threading.Event()
        self._first = True

    def run(self):
        while self._first or not self._stop_event.wait(self.interval):
            self._first = False
            value = big.sample_rss_mb(self.pid)
            if value is not None:
                self.samples += 1
                self.peak = max(self.peak, value)
                self.series.append(round(value, 1))
            if self._stop_event.is_set():
                break

    def stop(self):
        self._stop_event.set()


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pir", default=os.path.join(REPO, "target-alt", "release", "pir.exe"))
    ap.add_argument("--model", default="openai/gpt-5")
    ap.add_argument("--thinking", default=None, help="off|low|... (omit for settings default)")
    ap.add_argument("--agent-dir", default=None,
                    help="PIR_AGENT_DIR override (default: the process default ~/.pi/agent; "
                         "the probe only reads it, the child runs with --no-session)")
    ap.add_argument("--interval", type=float, default=0.1,
                    help="root tasklist sampling interval in seconds")
    ap.add_argument("--tree-interval", type=float, default=0.25,
                    help="process-tree (CIM) sampling interval in seconds; each CIM query "
                         "costs ~0.7 s on this host, so this is a floor, not a promise")
    ap.add_argument("--no-tree", action="store_true",
                    help="skip the process-tree sampler (root pid only; labels the number "
                         "scope=root and cannot see tool/extension children)")
    ap.add_argument("--timeout", type=float, default=300.0)
    ap.add_argument("--label", default="probe")
    ap.add_argument("--arg", action="append", default=[],
                    help="extra CLI arg inserted before the prompt (repeatable)")
    ap.add_argument("--out", default=os.path.join(REPO, "logs", "perf", "rss"))
    ap.add_argument("prompt", nargs="?", default="say ok")
    args = ap.parse_args(argv)

    if not os.path.isfile(args.pir):
        print(f"ERROR: pir not found: {args.pir}", file=sys.stderr)
        return 2
    # ABSOLUTE path: Windows CreateProcess rejects a relative path that uses
    # forward slashes (`--pir target-alt/debug/pir.exe` raised WinError 2 here
    # before any measurement); os.path.abspath also normalizes separators.
    pir = os.path.abspath(args.pir)

    os.makedirs(args.out, exist_ok=True)
    out_path = os.path.join(args.out, f"{args.label}.stdout.txt")
    err_path = os.path.join(args.out, f"{args.label}.stderr.txt")
    env = dict(os.environ)
    if args.agent_dir:
        env["PIR_AGENT_DIR"] = args.agent_dir
    cmd = [pir, "print", "--no-session", "--model", args.model]
    if args.thinking:
        cmd += ["--thinking", args.thinking]
    cmd += args.arg
    cmd.append(args.prompt)

    started = time.time()
    with open(out_path, "wb") as out_fh, open(err_path, "wb") as err_fh:
        proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=out_fh, stderr=err_fh, env=env)
    sampler = None
    if not args.no_tree and os.name == "nt":
        sampler = TreeSampler(proc.pid, args.tree_interval)
        sampler.start()
    peak = 0.0
    samples = 0
    series = []
    timed_out = False
    root_sampled_at = -1.0
    while proc.poll() is None:
        if time.time() - started > args.timeout:
            proc.kill()
            timed_out = True
            break
        # Contract: the root `tasklist` point is cheap enough to run on the
        # requested interval and keeps the series non-empty when CIM is slow;
        # it is labelled scope=root, never merged into the tree peak.
        if time.time() - root_sampled_at >= args.interval or root_sampled_at < 0:
            root_sampled_at = time.time()
            rss = sample_root_rss_mb(proc.pid)
            if rss is not None:
                peak = max(peak, rss)
                samples += 1
                series.append(round(rss, 1))
        time.sleep(0.05)
    proc.wait(timeout=30)
    wall = time.time() - started
    if sampler is not None:
        sampler.stop()
        sampler.join(timeout=5)
    stderr = open(err_path, "r", encoding="utf-8", errors="replace").read()
    # Contract: scope names the instrument that produced `peak`. The verdict
    # prefers the tree (the real cost), falling back to root only when every
    # CIM query failed — and says so in the same line.
    if sampler is not None and sampler.samples > 0:
        peak, scope, n = sampler.peak, "tree", sampler.samples
        series = sampler.series
    elif samples > 0:
        scope, n = "root", samples
    else:
        scope, n = None, 0
    print(f"[rss] {args.label}: wall={wall:.2f}s peak={peak:.1f} MB scope={scope} "
          f"samples={n} exit={proc.returncode} timed_out={timed_out}")
    if series:
        print(f"[rss] series MB: {series[:4]} ... {series[-4:]}")
    ext_lines = [l for l in stderr.splitlines() if "[ext]" in l]
    for line in ext_lines[:6]:
        print(f"[rss] {line.strip()}")
    # Contract: a leg with no sample is not a 0.0 MB floor (that would rank as
    # the best possible result); a killed/failed child is not a floor either.
    if scope is None:
        print(f"[rss] INVALID: no RSS sample landed (exit={proc.returncode}); "
              f"the run is not measurable")
        return 1
    if timed_out or proc.returncode != 0:
        print(f"[rss] INVALID: the child did not finish cleanly "
              f"(exit={proc.returncode}, timed_out={timed_out}); the peak describes "
              f"a failed/partial run, not a one-delta floor")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
