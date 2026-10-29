#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""One-command mode scorecard for `pir` on Windows.

Why this exists (docs/STATUS.md, row "Mode deep optimization & competitive
scorecard"): the tree already has separate probes for pieces of the story —
`cli_cold_start.mjs` (cold start), `rss_probe.py` (print-mode peak RSS),
`big_session_probe.py` (large sessions), `tui_session_probe.py` (TUI memory),
`cold_start_attribution.py` (per-phase startup attribution).  None of them
produces one table over the modes the status row names, and **server idle**,
**env daemon round-trip**, **TUI first-frame**, **phone**, **list-models**,
**RPC round-trip** and **tool execution** had no unified probe.  This script
fills that gap with one rerunnable harness:

  cold        `pir --version` / `pir --help`, N fresh processes each
  list-models `pir --list-models` (mock agent, and mock + openrouter entry)
  print       `pir print` with the in-process mock provider (default / --no-tools)
  tools       `pir print` whose mock answers with one `read` / `bash` tool call
  rpc-idle    `pir rpc`: N sessions, idle RSS/CPU/threads + hello/prompt round-trips
  server      `pir server` start -> listening, idle RSS, `pir client` round-trip, stop
  env         `pir env-daemon serve`: sync line + hello + file round-trip
  tui         ConPTY first frame of `pir interactive` (ESC[?2026l marker)
  phone       `-e builtin:phone` TUI command -> localhost HTTP health latency

Every leg defaults to **5 runs** (`--*-runs`), and reports median/min/max plus
RSS/thread counters where the process lives long enough to sample them.

Reuse, not reinvention:
  * `big_session_probe.py` supplies the mock OpenAI-compatible provider, the
    sanitized agent-dir fixture, the process-tree RSS instrument
    (`sample_rss_mb` / `TreeSampler`) and the binary identity/staleness helpers.
  * `scripts/tui_conpty/tui_drive.py` supplies the real ConPTY driver.
  * CPU time and thread counts are read through ctypes (`GetProcessTimes`,
    `CreateToolhelp32Snapshot`), stdlib only; no psutil dependency.
  * The tool-call SSE shape follows `scripts/perf/body_probe.py` /
    `scripts/perf/fleet_simulation.py`; the RPC JSON-lines schema follows
    `src/coding_agent/rpc.rs` (hello/list/get_state/prompt/agent_settled).

Contracts:
  * A number is only printed when it was measured.  A leg that cannot run is
    emitted as **N/A / NOT MEASURABLE** with the reason — never as 0.
  * Every leg kills only the process tree it spawned (Popen handle pid;
    `taskkill /T /F`), never a process it did not start.
  * Raw stdout/stderr/log files are written under `<out>/raw-<stamp>/<mode>/`
    and every table row names its artifact.
  * The binary's size/mtime/SHA-256/version, its build kind (release/debug) and
    a stale-vs-source note are printed in the report.  A stale or debug binary
    makes the run a **smoke test**; final-status numbers must come from the
    release binary.

Usage:
  python scripts/perf/mode_scorecard.py [--pir target/release/pir.exe]
      [--out logs/lane-scorecard-final] [--cold-runs 5] [--print-runs 5]
      [--tools-runs 5] [--list-models-runs 5] [--rpc-runs 5] [--server-runs 5]
      [--env-runs 5] [--tui-runs 5] [--phone-runs 5]
      [--rpc-seconds 5] [--server-seconds 5]
      [--only cold,list-models,...] [--skip phone] [--report-name SCORECARD.md]

Requires Windows (ConPTY + Toolhelp32).  Exit code: 0 = report written (N/A
allowed), 2 = binary missing / no mode selected / bad args, 3 = unexpected
harness error.
"""

import argparse
import ctypes
import ctypes.wintypes as wt
import http.server
import json
import os
import queue
import re
import signal
import socket
import socketserver
import statistics
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)

import big_session_probe as big  # noqa: E402
sys.path.insert(0, os.path.join(REPO, "scripts", "tui_conpty"))
try:
    import tui_drive  # noqa: E402
except Exception as _exc:  # ConPTY driver only loads on Windows
    tui_drive = None
    TUI_IMPORT_ERROR = str(_exc)
else:
    TUI_IMPORT_ERROR = None

FRAME_END = b"\x1b[?2026l"          # synchronized-update reset = one complete frame
CREATE_NEW_PROCESS_GROUP = 0x00000200

MODES = ["cold", "list-models", "print", "tools", "rpc-idle", "server", "env", "tui", "phone"]

# ---------------------------------------------------------------------------
# Reference numbers.  They are NOT restated as current measurements; the
# generated report shows them only as an orientation delta.
# ---------------------------------------------------------------------------

# `logs/lane-scorecard/REPORT.md` — debug binary cef29fcc…, run while other
# lanes were compiling (cold max 1488.7 ms, print 2800-4342 ms).  Kept whole
# so the report can call out load contamination explicitly.
REF_OLD_SCORECARD_DEBUG = {
    "label": "old scorecard (debug, load-contaminated)",
    "binary": "target/debug/pir.exe 312,117,335 B sha256 cef29fcc… (2026-10-29 00:24)",
    "version_median_ms": 36.8, "version_min_ms": 26.9,
    "help_median_ms": 38.5, "help_min_ms": 27.8,
    "print_median_ms": 3846.9, "print_min_ms": 2800.2,
    "rpc_idle_peak_mb": 33.3, "rpc_idle_last_mb": 17.1, "rpc_idle_threads": 27,
    "server_startup_ms": 1168.5, "server_client_ms": 615.0,
    "env_sync_ms": 52.84, "env_hello_ms": 8.42, "env_file_total_ms": 9.17,
    "tui_first_frame_median_ms": 3172.9, "tui_first_frame_min_ms": 2178.8,
    "phone_start_health_ms": 565.7, "phone_health_median_ms": 0.57,
}

# `logs/lane-cold-start/REPORT.md` §5 — quiet-host debug rebuild at 11:03
# (sha256 67615291…), same harness family, no concurrent builds.
REF_CLEAN_DEBUG = {
    "label": "clean debug (lane-cold-start, 11:03)",
    "binary": "target/debug/pir.exe 309,776,896 B sha256 67615291… (2026-10-29 11:03)",
    "version_median_ms": 12.2, "help_median_ms": 16.1,
    "print_median_ms": 240.4, "print_no_extensions_median_ms": 176.5,
    "list_models_openrouter_median_ms": 99.5,
    "server_startup_ms": 50.7,
    "tui_first_frame_median_ms": 100.8,
}

# `logs/lane-cold-start/REPORT.md` §6 — the previous 0.0.1 release binary
# (67,489,280 B, 2026-10-28 20:43), one-point references only.
REF_OLD_RELEASE = {
    "label": "previous 0.0.1 release",
    "binary": "target/release/pir.exe 67,489,280 B (2026-10-28 20:43)",
    "version_median_ms": 10.5,
    "models_list_ms": 17.6,
    "list_models_median_ms": 61.7,
    "print_median_ms": 121.1,
    "rpc_idle_peak_mb": 16.7, "rpc_idle_threads": 23,
}


# ---------------------------------------------------------------------------
# small utilities
# ---------------------------------------------------------------------------

def now_stamp():
    return time.strftime("%Y%m%d-%H%M%S")


def fmt_ms(value):
    return "N/A" if value is None else f"{value:.1f} ms"


def fmt_mb(value):
    return "N/A" if value is None else f"{value:.1f} MB"


def fmt_num(value, digits=1):
    return "N/A" if value is None else f"{value:.{digits}f}"


def write_text(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(text)


def write_bytes(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as fh:
        fh.write(data)


def rel(path):
    try:
        return os.path.relpath(path, REPO).replace("\\", "/")
    except ValueError:
        return path


def kill_tree(pid):
    """Kill a process tree WE spawned.  Never accepts a pid from elsewhere."""
    if not pid:
        return False
    try:
        done = subprocess.run(["taskkill", "/T", "/F", "/PID", str(pid)],
                              capture_output=True, timeout=20)
        return done.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def stats(values):
    """median/min/max/n over the non-None values; all-None -> n=0."""
    vals = [float(v) for v in values if v is not None]
    if not vals:
        return {"n": 0, "median": None, "min": None, "max": None}
    return {
        "n": len(vals),
        "median": round(statistics.median(vals), 2),
        "min": round(min(vals), 2),
        "max": round(max(vals), 2),
    }


def detect_build_kind(pir, identity):
    path = os.path.abspath(pir).replace("\\", "/").lower()
    if "/release/" in path:
        return "release"
    if "/debug/" in path:
        return "debug"
    size = identity.get("size") if identity else None
    if size is not None:
        return "release" if size < 150_000_000 else "debug"
    return "unknown"


# ---------------------------------------------------------------------------
# Win32 counters: root RSS, root peak RSS, CPU seconds, live thread count
# ---------------------------------------------------------------------------

_k32 = ctypes.WinDLL("kernel32", use_last_error=True)
_psapi = ctypes.WinDLL("psapi", use_last_error=True)
_k32.OpenProcess.restype = wt.HANDLE
_k32.CreateToolhelp32Snapshot.restype = wt.HANDLE


class _PMC(ctypes.Structure):
    _fields_ = [
        ("cb", wt.DWORD), ("PageFaultCount", wt.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t), ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t), ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t),
    ]


class _TE32(ctypes.Structure):
    _fields_ = [
        ("dwSize", wt.DWORD), ("cntUsage", wt.DWORD),
        ("th32ThreadID", wt.DWORD), ("th32OwnerProcessID", wt.DWORD),
        ("tpBasePri", ctypes.c_long), ("tpDeltaPri", ctypes.c_long),
        ("dwFlags", wt.DWORD),
    ]


def _thread_count(pid):
    threads = 0
    snap = _k32.CreateToolhelp32Snapshot(4, 0)  # TH32CS_SNAPTHREAD
    if snap and snap != ctypes.c_void_p(-1).value:
        entry = _TE32()
        entry.dwSize = ctypes.sizeof(_TE32)
        ok = _k32.Thread32First(snap, ctypes.byref(entry))
        while ok:
            if entry.th32OwnerProcessID == pid:
                threads += 1
            ok = _k32.Thread32Next(snap, ctypes.byref(entry))
        _k32.CloseHandle(snap)
    return threads


class WinProbe:
    """A process handle kept open across a short-lived leg.

    GetProcessMemoryInfo on an open handle still reports PeakWorkingSetSize
    after the process exits, which is the only reliable RSS number for a
    10-20 ms `--version` process.  Thread counts are live-only.
    """

    def __init__(self, pid):
        self.pid = pid
        self.handle = _k32.OpenProcess(0x1000, False, pid)  # QUERY_LIMITED_INFORMATION

    def counters(self):
        if not self.handle:
            return None
        mem = _PMC()
        mem.cb = ctypes.sizeof(_PMC)
        if not _psapi.GetProcessMemoryInfo(self.handle, ctypes.byref(mem), mem.cb):
            return None
        created = wt.FILETIME(); exited = wt.FILETIME()
        kernel = wt.FILETIME(); user = wt.FILETIME()
        _k32.GetProcessTimes(self.handle, ctypes.byref(created), ctypes.byref(exited),
                             ctypes.byref(kernel), ctypes.byref(user))
        cpu = ((kernel.dwHighDateTime << 32 | kernel.dwLowDateTime)
               + (user.dwHighDateTime << 32 | user.dwLowDateTime)) / 1e7
        return {
            "rssMb": round(mem.WorkingSetSize / (1024.0 * 1024.0), 2),
            "peakMb": round(mem.PeakWorkingSetSize / (1024.0 * 1024.0), 2),
            "cpuSeconds": cpu,
            "threads": _thread_count(self.pid),
        }

    def peak_mb(self):
        if not self.handle:
            return None
        mem = _PMC()
        mem.cb = ctypes.sizeof(_PMC)
        if not _psapi.GetProcessMemoryInfo(self.handle, ctypes.byref(mem), mem.cb):
            return None
        return round(mem.PeakWorkingSetSize / (1024.0 * 1024.0), 2)

    def close(self):
        if self.handle:
            _k32.CloseHandle(self.handle)
            self.handle = None


def process_counters(pid):
    """(rss_mb, cpu_seconds, thread_count) for ONE pid, or None."""
    probe = WinProbe(pid)
    try:
        counters = probe.counters()
        if counters is None:
            return None
        return counters["rssMb"], counters["cpuSeconds"], counters["threads"]
    finally:
        probe.close()


class RootSampler(threading.Thread):
    """Samples one process's RSS/peak/threads at a tight cadence.

    Used for print/tool legs (70-150 ms) where the 50 ms TreeSampler gives too
    few thread snapshots.  The process handle is opened before the first
    sample and kept until the sampler stops, so the final peak survives exit.
    """

    def __init__(self, pid, interval=0.02):
        super().__init__(daemon=True)
        self.pid = pid
        self.interval = interval
        self.probe = WinProbe(pid)
        self.samples = []
        self._stop_event = threading.Event()

    def run(self):
        while not self._stop_event.is_set():
            counters = self.probe.counters()
            if counters is not None:
                self.samples.append(counters)
            self._stop_event.wait(self.interval)

    def stop(self):
        self._stop_event.set()
        self.join(timeout=3.0)
        peak = self.probe.peak_mb()
        self.probe.close()
        # A post-exit sample reports 0 threads; that is "not measured", not a
        # real thread count (a live process always has at least one thread).
        threads = [sample["threads"] for sample in self.samples if sample["threads"] > 0]
        return {
            "samples": len(self.samples),
            "peakMb": peak,
            "rssPeakMb": max([sample["rssMb"] for sample in self.samples], default=None),
            "threadsPeak": max(threads) if threads else None,
            "threadsLast": threads[-1] if threads else None,
        }


def sample_leg(pid, seconds, interval=0.25, tree=True):
    """Sample one idle leg: tree RSS peak/last, root CPU %, thread high/last.

    CPU % is percent of ONE core (the metric STATUS.md quotes), computed from
    the root process's kernel+user delta over the sampled window.
    """
    first = process_counters(pid)
    cpu0 = first[1] if first else None
    started = time.perf_counter()
    sampler = big.TreeSampler(pid, interval=max(0.1, interval)) if tree else None
    if sampler is not None:
        sampler.start()
    threads_peak = 0
    threads_last = 0
    root_peak = 0.0
    samples = 0
    while time.perf_counter() - started < seconds:
        counters = process_counters(pid)
        if counters:
            rss, _cpu, threads = counters
            samples += 1
            root_peak = max(root_peak, rss)
            threads_peak = max(threads_peak, threads)
            threads_last = threads
        time.sleep(interval)
    wall = time.perf_counter() - started
    last = process_counters(pid)
    if sampler is not None:
        sampler.stop()
    series = [round(sample[1], 1) for sample in sampler.samples] if sampler else []
    tree_peak = sampler.peak if sampler and sampler.samples else None
    tree_last = series[-1] if series else None
    cpu_pct = None
    if cpu0 is not None and last is not None and wall > 0:
        cpu_pct = max(0.0, (last[1] - cpu0) / wall * 100.0)
    if samples and tree_last is not None:
        peak, peak_scope = round(tree_peak, 1), "tree"
    elif samples:
        peak, peak_scope = round(root_peak, 1), "root"
    else:
        peak, peak_scope = None, None
    return {
        "seconds": round(wall, 2),
        "samples": samples,
        "rssPeakMb": peak,
        "rssScope": peak_scope,
        "rssLastMb": round(tree_last, 1) if tree_last is not None else None,
        "rootPeakMb": round(root_peak, 1) if samples else None,
        "cpuPctOneCore": round(cpu_pct, 2) if cpu_pct is not None else None,
        "threadsPeak": threads_peak if samples else None,
        "threadsLast": threads_last if samples else None,
        "seriesMb": series,
    }


# ---------------------------------------------------------------------------
# one short-lived process with root peak RSS / thread sampling
# ---------------------------------------------------------------------------

def peak_from_handle(handle):
    """PeakWorkingSetSize from an existing process handle (Popen `_handle`).

    Python's Popen keeps the CreateProcess handle open until the object is
    closed, so this works after process exit without an OpenProcess call.
    Opening a fresh handle right after spawn measurably slows the child
    (~45 ms on this host), so short legs use the Popen handle instead.
    """
    if not handle:
        return None
    mem = _PMC()
    mem.cb = ctypes.sizeof(_PMC)
    if not _psapi.GetProcessMemoryInfo(handle, ctypes.byref(mem), mem.cb):
        return None
    return round(mem.PeakWorkingSetSize / (1024.0 * 1024.0), 2)


def run_short_process(argv, env, out_path, err_path, timeout=180, sample_interval=0.005):
    """Run one short-lived process; wall time is read at process exit.

    A background RootSampler collects thread counts, but the wall clock is
    taken before any sampler shutdown join.  `Toolhelp32` thread enumeration
    costs ~57 ms on this host, so a synchronous probe would turn a 10 ms
    `--version` into a 70 ms number; the background sampler does not gate the
    child's wall time.
    """
    started = time.perf_counter()
    with open(out_path, "wb") as out_fh, open(err_path, "wb") as err_fh:
        proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                                stdout=out_fh, stderr=err_fh, env=env)
        sampler = RootSampler(proc.pid, interval=sample_interval)
        sampler.start()
        try:
            proc.wait(timeout=timeout)
            wall_ms = (time.perf_counter() - started) * 1000.0
        finally:
            root = sampler.stop()
            if proc.poll() is None:
                kill_tree(proc.pid)
                proc.wait(timeout=10)
        peak = peak_from_handle(getattr(proc, "_handle", None))
        if peak is None:
            peak = root["peakMb"]
    return {
        "wallMs": round(wall_ms, 2),
        "exit": proc.returncode,
        "peakRootMb": peak,
        "rssRootPeakMb": peak,
        "threadsPeak": root["threadsPeak"],
        "threadsLast": root["threadsLast"],
        "rssSamples": root["samples"],
        "stdout": rel(out_path),
        "stderr": rel(err_path),
    }


# ---------------------------------------------------------------------------
# mode: cold start
# ---------------------------------------------------------------------------

def run_cold(pir, mode_dir, runs):
    samples = {}
    rows = []
    for flag, label in ((["--version"], "version"), (["--help"], "help")):
        times = []
        for i in range(1, runs + 1):
            out_path = os.path.join(mode_dir, f"{label}_run{i:02d}.stdout.txt")
            err_path = os.path.join(mode_dir, f"{label}_run{i:02d}.stderr.txt")
            env = dict(os.environ)
            env["PIR_NO_UPDATE_CHECK"] = "1"
            env["RUST_LOG"] = "error"
            sample = run_short_process([pir] + flag, env, out_path, err_path)
            times.append(sample["wallMs"])
            rows.append({"flag": label, "run": i, **sample})
        samples[label] = {
            "timesMs": [round(t, 2) for t in times],
            "medianMs": round(statistics.median(times), 2),
            "minMs": round(min(times), 2),
            "maxMs": round(max(times), 2),
            "wall": stats(times),
            "peakRootMb": stats([r["peakRootMb"] for r in rows if r["flag"] == label]),
            "threadsPeak": stats([r["threadsPeak"] for r in rows if r["flag"] == label]),
        }
    return samples, rows


# ---------------------------------------------------------------------------
# mock provider shared by print / rpc-idle / tui / phone
# ---------------------------------------------------------------------------

class MockProvider:
    def __init__(self):
        self.server = big.Mock(("127.0.0.1", 0), big._Mock)
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def agent_dir(self):
        return big.build_agent_dir(self.port, 128000)

    def close(self):
        try:
            self.server.shutdown()
        except Exception:
            pass


def base_child_env(agent):
    env = dict(os.environ)
    env["PIR_AGENT_DIR"] = agent
    env["PI_CODING_AGENT_DIR"] = agent
    env["PIR_NO_UPDATE_CHECK"] = "1"
    env["RUST_LOG"] = "error"
    return env


def agent_with_openrouter(mock):
    """Mock agent dir plus an `openrouter` models.json entry (builtin id).

    Reproduces the fixture `cold_start_attribution.py` used for the parse-once
    catalog work: the provider id is a builtin, so a duplicate re-parse of the
    embedded catalog shows up in `--list-models` latency.
    """
    agent = mock.agent_dir()
    path = os.path.join(agent, "models.json")
    with open(path, encoding="utf-8") as fh:
        cfg = json.load(fh)
    cfg["providers"]["openrouter"] = {
        "name": "OpenRouter",
        "baseUrl": "https://openrouter.ai/api/v1",
        "api": "openai-completions",
        "apiKey": "sk-mock-openrouter",
        "models": [{
            "id": "mock/or-model", "name": "OR", "reasoning": False,
            "input": ["text"], "contextWindow": 128000, "maxTokens": 4096,
        }],
    }
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        json.dump(cfg, fh, indent=2)
    return agent


# ---------------------------------------------------------------------------
# mode: --list-models
# ---------------------------------------------------------------------------

def run_list_models(pir, mode_dir, mock, runs):
    variants = {}
    for variant in ("mock", "openrouter"):
        agent = mock.agent_dir() if variant == "mock" else agent_with_openrouter(mock)
        env = base_child_env(agent)
        rows = []
        for i in range(1, runs + 1):
            out_path = os.path.join(mode_dir, f"list_models_{variant}_run{i:02d}.stdout.txt")
            err_path = os.path.join(mode_dir, f"list_models_{variant}_run{i:02d}.stderr.txt")
            sample = run_short_process([pir, "--list-models"], env, out_path, err_path)
            try:
                with open(out_path, encoding="utf-8", errors="replace") as fh:
                    sample["stdoutLines"] = sum(1 for _ in fh)
            except OSError:
                sample["stdoutLines"] = None
            sample["run"] = i
            rows.append(sample)
        variants[variant] = {
            "runs": rows,
            "wall": stats([r["wallMs"] for r in rows if r["exit"] == 0]),
            "peakRootMb": stats([r["peakRootMb"] for r in rows if r["exit"] == 0]),
            "threadsPeak": stats([r["threadsPeak"] for r in rows if r["exit"] == 0]),
            "allExitedZero": all(r["exit"] == 0 for r in rows),
            "stdoutLines": rows[0]["stdoutLines"] if rows else None,
            "status": "measured" if any(r["exit"] == 0 for r in rows) else "N/A",
            "command": f'"{pir}" --list-models  (PIR_AGENT_DIR=<temp>)',
            "rawDir": rel(mode_dir),
        }
    return {"status": "measured", "variants": variants, "rawDir": rel(mode_dir)}


# ---------------------------------------------------------------------------
# mode: print (mock provider; default and --no-tools)
# ---------------------------------------------------------------------------

def _run_print_variant(pir, mode_dir, mock, runs, variant, extra):
    agent = mock.agent_dir()
    env = base_child_env(agent)
    samples = []
    for i in range(1, runs + 1):
        out_path = os.path.join(mode_dir, f"print_{variant}_run{i:02d}.stdout.txt")
        err_path = os.path.join(mode_dir, f"print_{variant}_run{i:02d}.stderr.txt")
        argv = [pir, "print", "--no-session", "--model", "mock/mock-model"] + extra + ["Reply with OK"]
        started = time.perf_counter()
        with open(out_path, "wb") as out_fh, open(err_path, "wb") as err_fh:
            proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                                    stdout=out_fh, stderr=err_fh, env=env)
            tree = big.TreeSampler(proc.pid)
            tree.start()
            root = RootSampler(proc.pid, interval=0.02)
            root.start()
            try:
                proc.wait(timeout=180)
            finally:
                wall = (time.perf_counter() - started) * 1000.0
                root_stats = root.stop()
                tree.stop()
                if proc.poll() is None:
                    kill_tree(proc.pid)
                    proc.wait(timeout=10)
        try:
            with open(out_path, encoding="utf-8", errors="replace") as fh:
                stdout_text = fh.read()
        except OSError:
            stdout_text = ""
        samples.append({
            "run": i,
            "exit": proc.returncode,
            "wallMs": round(wall, 1),
            "peakTreeMb": round(tree.peak, 1) if tree.samples else None,
            "peakRootMb": root_stats["peakMb"],
            "threadsPeak": root_stats["threadsPeak"],
            "threadsLast": root_stats["threadsLast"],
            "rssSamples": len(tree.samples),
            "stdoutHasOK": "OK" in stdout_text,
            "stdout": rel(out_path),
            "stderr": rel(err_path),
        })
    walls = [s["wallMs"] for s in samples if s["exit"] == 0]
    peaks = [s["peakTreeMb"] for s in samples if s["peakTreeMb"] is not None]
    return {
        "status": "measured" if walls else "N/A",
        "reason": None if walls else "every print leg failed (see raw stderr)",
        "runs": samples,
        "wall": stats(walls),
        "medianWallMs": round(statistics.median(walls), 1) if walls else None,
        "minWallMs": round(min(walls), 1) if walls else None,
        "maxWallMs": round(max(walls), 1) if walls else None,
        "peakRssMb": round(max(peaks), 1) if peaks else None,
        "peakTreeMb": stats([s["peakTreeMb"] for s in samples]),
        "peakRootMb": stats([s["peakRootMb"] for s in samples]),
        "threadsPeak": stats([s["threadsPeak"] for s in samples]),
        "command": f'"{pir}" print --no-session --model mock/mock-model {" ".join(extra)} "Reply with OK"',
    }


def run_print(pir, mode_dir, mock, runs):
    variants = {}
    for variant, extra in (("default", []), ("no-tools", ["--no-tools"])):
        variants[variant] = _run_print_variant(pir, mode_dir, mock, runs, variant, extra)
    return {
        "status": "measured" if any(v["status"] == "measured" for v in variants.values()) else "N/A",
        "variants": variants,
    }


# ---------------------------------------------------------------------------
# mode: tool execution (one read/bash call per print run)
# ---------------------------------------------------------------------------

def _sse(payload):
    return ("data: " + json.dumps(payload) + "\n\n").encode("utf-8")


class ToolMockHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_GET(self):
        body = json.dumps({"object": "list", "data": [{"id": "mock-model", "object": "model"}]})
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body.encode("utf-8"))

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        server = self.server
        with server.lock:
            server.reqs += 1
            request_index = server.reqs
            server.bodies.append(body[:500_000])
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        created = int(time.time())
        try:
            if request_index == 1:
                name, arguments = server.tool
                self.wfile.write(_sse({
                    "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""},
                                 "finish_reason": None}],
                }))
                self.wfile.write(_sse({
                    "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": {"tool_calls": [{
                        "index": 0, "id": "call_tool_1", "type": "function",
                        "function": {"name": name, "arguments": json.dumps(arguments)},
                    }]}, "finish_reason": None}],
                }))
                self.wfile.write(_sse({
                    "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                }))
            else:
                for delta in ({"role": "assistant", "content": ""}, {"content": "DONE"}):
                    self.wfile.write(_sse({
                        "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                        "model": "mock-model",
                        "choices": [{"index": 0, "delta": delta, "finish_reason": None}],
                    }))
                self.wfile.write(_sse({
                    "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                }))
            self.wfile.write(b"data: [DONE]\n\n")
        except (BrokenPipeError, ConnectionResetError):
            return


class ToolMockServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def run_tools(pir, mode_dir, runs):
    fixture_dir = os.path.join(mode_dir, "work")
    os.makedirs(fixture_dir, exist_ok=True)
    fixture_path = os.path.join(fixture_dir, "tool_fixture.txt")
    marker = "TOOL-BENCH-READ-OK"
    with open(fixture_path, "w", encoding="utf-8", newline="\n") as fh:
        for i in range(200):
            fh.write(f"{marker} line {i:03d} lorem ipsum dolor sit amet\n")

    tools = {}
    for tool, arguments, expect_marker in (
        ("read", {"path": fixture_path.replace("\\", "/"), "limit": 40, "offset": 1}, marker),
        ("bash", {"command": "echo TOOL-BENCH-BASH-OK"}, "TOOL-BENCH-BASH-OK"),
    ):
        samples = []
        for i in range(1, runs + 1):
            server = ToolMockServer(("127.0.0.1", 0), ToolMockHandler)
            server.reqs = 0
            server.bodies = []
            server.lock = threading.Lock()
            server.tool = (tool, arguments)
            threading.Thread(target=server.serve_forever, daemon=True).start()
            try:
                agent = big.build_agent_dir(server.server_address[1], 128000)
                env = base_child_env(agent)
                out_path = os.path.join(mode_dir, f"tools_{tool}_run{i:02d}.stdout.txt")
                err_path = os.path.join(mode_dir, f"tools_{tool}_run{i:02d}.stderr.txt")
                argv = [pir, "print", "--no-session", "--model", "mock/mock-model",
                        "--tools", tool, "Use the tool."]
                started = time.perf_counter()
                with open(out_path, "wb") as out_fh, open(err_path, "wb") as err_fh:
                    proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                                            stdout=out_fh, stderr=err_fh, env=env)
                    tree = big.TreeSampler(proc.pid)
                    tree.start()
                    root = RootSampler(proc.pid, interval=0.02)
                    root.start()
                    try:
                        proc.wait(timeout=180)
                    finally:
                        wall = (time.perf_counter() - started) * 1000.0
                        root_stats = root.stop()
                        tree.stop()
                        if proc.poll() is None:
                            kill_tree(proc.pid)
                            proc.wait(timeout=10)
                try:
                    with open(out_path, encoding="utf-8", errors="replace") as fh:
                        stdout_text = fh.read()
                except OSError:
                    stdout_text = ""
                second_body = b""
                with server.lock:
                    if len(server.bodies) >= 2:
                        second_body = server.bodies[1]
                samples.append({
                    "run": i,
                    "exit": proc.returncode,
                    "wallMs": round(wall, 1),
                    "peakTreeMb": round(tree.peak, 1) if tree.samples else None,
                    "peakRootMb": root_stats["peakMb"],
                    "threadsPeak": root_stats["threadsPeak"],
                    "requests": server.reqs,
                    "toolResultSeen": expect_marker.encode() in second_body,
                    "stdoutHasDONE": "DONE" in stdout_text,
                    "stdout": rel(out_path),
                    "stderr": rel(err_path),
                })
            finally:
                server.shutdown()
        walls = [s["wallMs"] for s in samples if s["exit"] == 0]
        tools[tool] = {
            "status": "measured" if walls and all(s["toolResultSeen"] for s in samples) else "N/A",
            "runs": samples,
            "wall": stats(walls),
            "peakTreeMb": stats([s["peakTreeMb"] for s in samples]),
            "peakRootMb": stats([s["peakRootMb"] for s in samples]),
            "threadsPeak": stats([s["threadsPeak"] for s in samples]),
            "command": f'"{pir}" print --no-session --model mock/mock-model --tools {tool} "Use the tool."',
        }
    return {"status": "measured" if all(v["status"] == "measured" for v in tools.values()) else "N/A",
            "tools": tools, "rawDir": rel(mode_dir)}


# ---------------------------------------------------------------------------
# mode: idle RPC + JSON-lines round-trip
# ---------------------------------------------------------------------------

class _LinePump(threading.Thread):
    def __init__(self, stream):
        super().__init__(daemon=True)
        self.stream = stream
        self.lines = queue.Queue()
        self.seen = []

    def run(self):
        try:
            for line in iter(self.stream.readline, ""):
                self.seen.append(line)
                self.lines.put(line)
        except (OSError, ValueError):
            pass
        finally:
            self.lines.put(None)


def _wait_json(pump, predicate, timeout):
    deadline = time.perf_counter() + timeout
    while time.perf_counter() < deadline:
        remaining = max(0.01, deadline - time.perf_counter())
        try:
            line = pump.lines.get(timeout=remaining)
        except queue.Empty:
            break
        if line is None:
            break
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if predicate(obj):
            return obj
    return None


def run_rpc_idle(pir, mode_dir, mock, runs, seconds):
    session_runs = []
    all_hello = []
    all_prompt = []
    for i in range(1, runs + 1):
        run_dir = os.path.join(mode_dir, f"run{i:02d}")
        os.makedirs(run_dir, exist_ok=True)
        agent = mock.agent_dir()
        env = base_child_env(agent)
        out_path = os.path.join(run_dir, "rpc.stdout.txt")
        err_path = os.path.join(run_dir, "rpc.stderr.txt")
        with open(err_path, "wb") as err_fh:
            proc = subprocess.Popen(
                [pir, "rpc", "--no-session", "--model", "mock/mock-model"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err_fh,
                env=env, text=True, bufsize=1,
            )
            pump = _LinePump(proc.stdout)
            pump.start()
            sample = {"run": i, "status": "N/A", "reason": None}
            try:
                time.sleep(0.6)
                if proc.poll() is not None:
                    sample["reason"] = f"rpc exited before sampling (exit={proc.returncode})"
                else:
                    idle = sample_leg(proc.pid, seconds)
                    latencies = []
                    for j in range(10):
                        started = time.perf_counter()
                        proc.stdin.write(json.dumps({"type": "hello", "id": f"h{j}"}) + "\n")
                        proc.stdin.flush()
                        reply = _wait_json(
                            pump,
                            lambda obj: (obj.get("type") == "response"
                                         and obj.get("command") == "hello"
                                         and obj.get("id") == f"h{j}"),
                            timeout=10,
                        )
                        if reply is None:
                            raise TimeoutError(f"no hello reply {j}")
                        latencies.append(round((time.perf_counter() - started) * 1000.0, 3))
                    # Steady-state aggregate excludes each session's first
                    # hello (the connection/first-command warm-up); the
                    # per-run first value is reported separately.
                    all_hello.extend(latencies[1:])
                    # One full prompt round-trip: response line + agent_settled.
                    prompt_started = time.perf_counter()
                    proc.stdin.write(json.dumps(
                        {"type": "prompt", "id": "p1", "text": "Reply with OK"}) + "\n")
                    proc.stdin.flush()
                    prompt_reply = _wait_json(
                        pump,
                        lambda obj: (obj.get("type") == "response"
                                     and obj.get("command") == "prompt"
                                     and obj.get("id") == "p1"),
                        timeout=30,
                    )
                    prompt_ack_ms = ((time.perf_counter() - prompt_started) * 1000.0
                                     if prompt_reply is not None else None)
                    settled = _wait_json(
                        pump, lambda obj: obj.get("type") == "agent_settled", timeout=120)
                    settled_ms = ((time.perf_counter() - prompt_started) * 1000.0
                                  if settled is not None else None)
                    if settled_ms is not None:
                        all_prompt.append(settled_ms)
                    after = process_counters(proc.pid)
                    sample.update({
                        "status": "measured" if latencies else "N/A",
                        "idle": idle,
                        "helloLatenciesMs": latencies,
                        "firstHelloMs": latencies[0] if latencies else None,
                        "helloMedianMs": round(statistics.median(latencies), 3) if latencies else None,
                        "helloMinMs": min(latencies) if latencies else None,
                        "helloMaxMs": max(latencies) if latencies else None,
                        "promptAckMs": round(prompt_ack_ms, 2) if prompt_ack_ms is not None else None,
                        "promptSettledMs": round(settled_ms, 2) if settled_ms is not None else None,
                        "promptOk": bool(prompt_reply and prompt_reply.get("success") and settled),
                        "rssAfterPromptMb": round(after[0], 1) if after else None,
                        "threadsAfterPrompt": after[2] if after else None,
                    })
            except Exception as exc:
                sample["reason"] = f"rpc leg failed: {exc}"
            finally:
                try:
                    proc.stdin.close()
                except OSError:
                    pass
                try:
                    proc.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    kill_tree(proc.pid)
                    proc.wait(timeout=10)
                sample["exit"] = proc.returncode
                sample["stdout"] = rel(out_path)
                sample["stderr"] = rel(err_path)
                try:
                    write_text(out_path, "".join(pump.seen))
                except OSError:
                    pass
        session_runs.append(sample)
    idle_stats = [s["idle"] for s in session_runs if s.get("idle")]
    return {
        "status": "measured" if idle_stats else "N/A",
        "reason": None if idle_stats else "every rpc session failed",
        "runs": session_runs,
        "idlePeakMb": stats([s["rssPeakMb"] for s in idle_stats]),
        "idleLastMb": stats([s["rssLastMb"] for s in idle_stats]),
        "cpuPct": stats([s["cpuPctOneCore"] for s in idle_stats]),
        "threads": stats([s["threadsLast"] for s in idle_stats]),
        "firstHelloMs": stats([s["firstHelloMs"] for s in session_runs]),
        "helloSteadyMs": stats(all_hello),
        "helloAllMs": stats(all_hello),
        "promptSettledMs": stats(all_prompt),
        "command": f'"{pir}" rpc --no-session --model mock/mock-model  (PIR_AGENT_DIR=<temp>); '
                   f'{runs} sessions x {seconds:g} s idle, 10 hello + 1 prompt each',
    }


# ---------------------------------------------------------------------------
# mode: server start/idle/client/stop
# ---------------------------------------------------------------------------

def _read_lines(stream, sink):
    for line in iter(stream.readline, b""):
        sink.append(line.decode("utf-8", errors="replace"))


def run_server_once(pir, mode_dir, seconds, run_index):
    run_dir = os.path.join(mode_dir, f"run{run_index:02d}")
    agent = os.path.join(run_dir, "agent")
    sessions = os.path.join(run_dir, "sessions")
    os.makedirs(agent, exist_ok=True)
    os.makedirs(sessions, exist_ok=True)
    env = dict(os.environ)
    env["PIR_AGENT_DIR"] = agent
    env["PI_CODING_AGENT_DIR"] = agent
    env["PIR_SESSION_DIR"] = sessions
    env["PIR_SERVER_ADDRESS"] = "127.0.0.1:0"
    env["PIR_NO_UPDATE_CHECK"] = "1"
    env["RUST_LOG"] = "error"
    out_path = os.path.join(run_dir, "server.stdout.txt")
    err_path = os.path.join(run_dir, "server.stderr.txt")
    stderr_lines = []
    result = {"status": "N/A", "run": run_index, "stdout": rel(out_path), "stderr": rel(err_path)}
    proc = None
    with open(out_path, "wb") as out_fh:
        proc = subprocess.Popen(
            [pir, "server", "--no-extensions"],
            stdin=subprocess.DEVNULL, stdout=out_fh, stderr=subprocess.PIPE,
            env=env, creationflags=CREATE_NEW_PROCESS_GROUP,
        )
        reader = threading.Thread(target=_read_lines, args=(proc.stderr, stderr_lines), daemon=True)
        reader.start()
        started = time.perf_counter()
        address = None
        deadline = started + 30
        while time.perf_counter() < deadline:
            for line in list(stderr_lines):
                match = re.search(r"listening on (\S+)", line)
                if match:
                    address = match.group(1)
                    break
            if address or proc.poll() is not None:
                break
            time.sleep(0.02)
        startup_ms = (time.perf_counter() - started) * 1000.0
        if address and proc.poll() is None:
            idle = sample_leg(proc.pid, seconds)
            client_started = time.perf_counter()
            client_env = dict(env)
            client_env["PIR_SERVER_ADDRESS"] = address
            client = subprocess.run(
                [pir, "client", "--no-extensions"], stdin=subprocess.DEVNULL,
                capture_output=True, text=True, timeout=60, env=client_env,
            )
            client_ms = (time.perf_counter() - client_started) * 1000.0
            write_text(os.path.join(run_dir, "client.stdout.txt"), client.stdout)
            write_text(os.path.join(run_dir, "client.stderr.txt"), client.stderr)
            stop_started = time.perf_counter()
            stop_method = "none"
            if proc.poll() is None:
                try:
                    proc.send_signal(signal.CTRL_BREAK_EVENT)
                    stop_method = "CTRL_BREAK"
                except (OSError, ValueError):
                    pass
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    kill_tree(proc.pid)
                    proc.wait(timeout=10)
                    stop_method = "taskkill /T /F (forced)"
            stop_ms = (time.perf_counter() - stop_started) * 1000.0
            result.update({
                "status": "measured",
                "address": address,
                "startupMs": round(startup_ms, 1),
                "idle": idle,
                "clientMs": round(client_ms, 1),
                "clientExit": client.returncode,
                "clientOutput": client.stdout.strip()[:200],
                "clientStdout": rel(os.path.join(run_dir, "client.stdout.txt")),
                "clientStderr": rel(os.path.join(run_dir, "client.stderr.txt")),
                "stopMs": round(stop_ms, 1),
                "stopMethod": stop_method,
                "serverExit": proc.returncode,
            })
        else:
            result["reason"] = (f"server did not print a listening line within 30 s "
                                f"(exit={proc.poll()}); stderr={''.join(stderr_lines)[-500:]!r}")
        if proc.poll() is None:
            kill_tree(proc.pid)
            proc.wait(timeout=10)
        write_text(err_path, "".join(stderr_lines))
    if proc is not None:
        result["exit"] = proc.returncode
    return result


def run_server(pir, mode_dir, seconds, runs):
    runs_out = [run_server_once(pir, mode_dir, seconds, i) for i in range(1, runs + 1)]
    ok = [r for r in runs_out if r["status"] == "measured"]
    return {
        "status": "measured" if ok else "N/A",
        "reason": None if ok else "every server leg failed",
        "runs": runs_out,
        "startupMs": stats([r["startupMs"] for r in ok]),
        "clientMs": stats([r["clientMs"] for r in ok]),
        "stopMs": stats([r["stopMs"] for r in ok]),
        "idlePeakMb": stats([r["idle"]["rssPeakMb"] for r in ok]),
        "idleLastMb": stats([r["idle"]["rssLastMb"] for r in ok]),
        "cpuPct": stats([r["idle"]["cpuPctOneCore"] for r in ok]),
        "threads": stats([r["idle"]["threadsLast"] for r in ok]),
        "command": f'PIR_SERVER_ADDRESS=127.0.0.1:0 "{pir}" server --no-extensions  '
                   f'then "{pir}" client --no-extensions (ephemeral loopback)',
    }


# ---------------------------------------------------------------------------
# mode: env daemon round-trip (upstream protocol v1: u32 len | u8 type |
# u32 id | u32 jsonLen | json | payload)
# ---------------------------------------------------------------------------

def _frame_bytes(kind, frame_id, obj, payload=b""):
    raw = json.dumps(obj, separators=(",", ":")).encode("utf-8")
    body = bytes([kind]) + struct.pack(">II", frame_id, len(raw)) + raw + payload
    return struct.pack(">I", len(body)) + body


def run_env_once(pir, mode_dir, run_index):
    run_dir = os.path.join(mode_dir, f"run{run_index:02d}")
    os.makedirs(run_dir, exist_ok=True)
    token = "0123456789abcdef0123456789abcdef"
    out_path = os.path.join(run_dir, "daemon.transcript.txt")
    err_path = os.path.join(run_dir, "daemon.stderr.txt")
    events = queue.Queue()
    transcript = []

    def reader(stream):
        try:
            sync = stream.readline()
            events.put(("sync", sync, time.perf_counter()))
            while True:
                header = stream.read(4)
                if len(header) < 4:
                    break
                length = struct.unpack(">I", header)[0]
                rest = b""
                while len(rest) < length:
                    chunk = stream.read(length - len(rest))
                    if not chunk:
                        break
                    rest += chunk
                if len(rest) < length:
                    break
                kind = rest[0]
                frame_id = struct.unpack(">I", rest[1:5])[0]
                json_length = struct.unpack(">I", rest[5:9])[0]
                obj = json.loads(rest[9:9 + json_length] or b"{}")
                events.put(("frame", (kind, frame_id, obj, rest[9 + json_length:]),
                            time.perf_counter()))
        except (OSError, ValueError):
            pass
        events.put(("eof", None, time.perf_counter()))

    with open(out_path, "w", encoding="utf-8", newline="\n") as out_fh, \
            open(err_path, "wb") as err_fh:
        spawn_perf = time.perf_counter()
        proc = subprocess.Popen(
            [pir, "env-daemon", "serve", "--token", token],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err_fh,
        )
        threading.Thread(target=reader, args=(proc.stdout,), daemon=True).start()
        result = {"status": "N/A", "run": run_index, "stdout": rel(out_path), "stderr": rel(err_path),
                  "command": f'"{pir}" env-daemon serve --token <32-hex>'}
        stats0 = None
        try:
            kind, value, stamp = events.get(timeout=20)
            sync_ms = (stamp - spawn_perf) * 1000.0
            transcript.append(f"sync line after {sync_ms:.1f} ms: {value!r}")
            if kind != "sync" or not value.startswith(b"PI-ENV "):
                result["reason"] = f"no PI-ENV sync line (got {kind}: {value!r})"
            else:
                stats0 = process_counters(proc.pid)
                frame_id = 1

                def call(name, obj, payload=b""):
                    nonlocal frame_id
                    this_id = frame_id
                    frame_id += 1
                    started = time.perf_counter()
                    proc.stdin.write(_frame_bytes(1, this_id, obj, payload))
                    proc.stdin.flush()
                    deadline = time.perf_counter() + 20
                    while time.perf_counter() < deadline:
                        try:
                            kind2, value2, _stamp2 = events.get(timeout=deadline - time.perf_counter())
                        except queue.Empty:
                            break
                        if kind2 == "frame" and value2[1] == this_id:
                            return round((time.perf_counter() - started) * 1000.0, 2), value2
                    raise TimeoutError(f"no reply to {name}")

                hello_ms, (hk, _hid, hello, _payload) = call("hello", {"op": "hello"})
                transcript.append(f"hello reply after {hello_ms} ms: {hello}")
                work = os.path.join(run_dir, "work")
                os.makedirs(work, exist_ok=True)
                path = os.path.join(work, "roundtrip.txt").replace("\\", "/")
                payload = b"mode-scorecard-env-roundtrip"
                file_started = time.perf_counter()
                write_ms, _ = call("write", {"op": "write", "path": path, "parents": True}, payload)
                stat_ms, (_k, _i, stat, _p) = call("lstat", {"op": "lstat", "path": path})
                open_ms, (_k2, _i2, opened, _p2) = call("open", {"op": "open", "path": path, "mode": "read"})
                handle = opened.get("handle")
                read_ms, (_k3, _i3, _r3, read_payload) = call(
                    "pread", {"op": "pread", "handle": handle, "length": 64})
                close_ms, _ = call("close", {"op": "close", "handle": handle})
                file_total_ms = (time.perf_counter() - file_started) * 1000.0
                ok = (hk == 2 and stat.get("size") == len(payload) and read_payload == payload)
                transcript.append(
                    f"file round-trip: write {write_ms} ms, lstat {stat_ms} ms, open {open_ms} ms, "
                    f"pread {read_ms} ms, close {close_ms} ms, total {file_total_ms:.2f} ms, "
                    f"payload_ok={ok}")
                result.update({
                    "status": "measured" if ok else "N/A",
                    "reason": None if ok else "round-trip payload/size mismatch",
                    "protocol": hello.get("protocol"),
                    "daemonVersion": hello.get("version"),
                    "syncMs": round(sync_ms, 2),
                    "helloMs": hello_ms,
                    "writeMs": write_ms,
                    "lstatMs": stat_ms,
                    "openMs": open_ms,
                    "preadMs": read_ms,
                    "closeMs": close_ms,
                    "fileTotalMs": round(file_total_ms, 2),
                    "roundTripOk": ok,
                    "rssMb": round(stats0[0], 1) if stats0 else None,
                    "threads": stats0[2] if stats0 else None,
                })
        except Exception as exc:
            result["reason"] = f"env-daemon leg failed: {exc}"
        finally:
            try:
                proc.stdin.close()
            except OSError:
                pass
            try:
                proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                kill_tree(proc.pid)
                proc.wait(timeout=10)
            result["exit"] = proc.returncode
            transcript.append(f"daemon exit {proc.returncode}")
            try:
                out_fh.write("\n".join(transcript) + "\n")
            except OSError:
                pass
    return result


def run_env(pir, mode_dir, runs):
    runs_out = [run_env_once(pir, mode_dir, i) for i in range(1, runs + 1)]
    ok = [r for r in runs_out if r["status"] == "measured"]
    return {
        "status": "measured" if ok else "N/A",
        "reason": None if ok else "every env-daemon leg failed",
        "runs": runs_out,
        "syncMs": stats([r["syncMs"] for r in ok]),
        "helloMs": stats([r["helloMs"] for r in ok]),
        "fileTotalMs": stats([r["fileTotalMs"] for r in ok]),
        "rssMb": stats([r["rssMb"] for r in ok]),
        "threads": stats([r["threads"] for r in ok]),
        "command": f'"{pir}" env-daemon serve --token <32-hex>  (protocol-v1 frames)',
    }


# ---------------------------------------------------------------------------
# mode: TUI first frame (ConPTY)
# ---------------------------------------------------------------------------

def _launch_tui(pir, agent, cwd, extra_args, out_log):
    cmdline = f'"{pir}" {extra_args} interactive --no-session'
    env = dict(tui_drive.BASE_ENV)
    env.update({"PIR_AGENT_DIR": agent, "PI_CODING_AGENT_DIR": agent,
                "PIR_NO_UPDATE_CHECK": "1", "RUST_LOG": "error"})
    started = time.perf_counter()
    pty = tui_drive.Pty(cmdline, cwd, env)
    setup_ms = (time.perf_counter() - started) * 1000.0
    return pty, setup_ms, started, env


def _wait_first_frame(pty, started, timeout_s=90):
    first_byte_ms = None
    first_frame_ms = None
    deadline = started + timeout_s
    while time.perf_counter() < deadline:
        data = bytes(pty.snapshot())
        if first_byte_ms is None and data:
            first_byte_ms = (time.perf_counter() - started) * 1000.0
        if FRAME_END in data:
            first_frame_ms = (time.perf_counter() - started) * 1000.0
            break
        if not pty.running():
            break
        time.sleep(0.01)
    return first_byte_ms, first_frame_ms


def _quit_tui(pty):
    try:
        pty.write(b"/quit\r")
        if pty.wait(6):
            return True
        pty.write(b"\x03")
        if pty.wait(3):
            return True
    except OSError:
        pass
    try:
        if pty.running():
            tui_drive.kill_process_tree(pty.pid)
    except Exception:
        pass
    return False


def run_tui(pir, mode_dir, mock, runs):
    agent = mock.agent_dir()
    work = os.path.join(mode_dir, "work")
    os.makedirs(work, exist_ok=True)
    samples = []
    for i in range(1, runs + 1):
        log_path = os.path.join(mode_dir, f"tui_run{i:02d}.pty.log")
        pty = None
        try:
            pty, setup_ms, started, _env = _launch_tui(pir, agent, work, "", log_path)
            sampler = big.TreeSampler(pty.pid)
            sampler.start()
            first_byte_ms, first_frame_ms = _wait_first_frame(pty, started)
            first_rss = sampler.mark()
            counters = process_counters(pty.pid)
            _quit_tui(pty)
            sampler.stop()
            samples.append({
                "run": i,
                "setupMs": round(setup_ms, 1),
                "firstByteMs": round(first_byte_ms, 1) if first_byte_ms is not None else None,
                "firstFrameMs": round(first_frame_ms, 1) if first_frame_ms is not None else None,
                "rssAtFirstFrameMb": first_rss,
                "rssNowMb": round(counters[0], 1) if counters else None,
                "threadsAtFrame": counters[2] if counters else None,
                "bytes": len(pty.snapshot()),
                "log": rel(log_path),
            })
        finally:
            if pty is not None:
                try:
                    if pty.running():
                        tui_drive.kill_process_tree(pty.pid)
                except Exception:
                    pass
                try:
                    pty.terminate()
                except Exception:
                    pass
                try:
                    pty.close()
                except Exception:
                    pass
                try:
                    write_bytes(log_path, bytes(pty.snapshot()))
                except Exception:
                    pass
    frames = [s["firstFrameMs"] for s in samples if s["firstFrameMs"] is not None]
    return {
        "status": "measured" if frames else "N/A",
        "reason": None if frames else "no complete frame (ESC[?2026l) before timeout",
        "runs": samples,
        "firstFrameMs": stats(frames),
        "firstByteMs": stats([s["firstByteMs"] for s in samples]),
        "setupMs": stats([s["setupMs"] for s in samples]),
        "rssAtFirstFrameMb": stats([s["rssAtFirstFrameMb"] for s in samples]),
        "threadsAtFrame": stats([s["threadsAtFrame"] for s in samples]),
        "medianFirstFrameMs": round(statistics.median(frames), 1) if frames else None,
        "minFirstFrameMs": round(min(frames), 1) if frames else None,
        "command": f'"{pir}" interactive --no-session  (ConPTY 120x40, PIR_AGENT_DIR=<temp>)',
    }


# ---------------------------------------------------------------------------
# mode: phone (localhost only; a real phone/WebSocket UI is NOT measurable here)
# ---------------------------------------------------------------------------

def _free_port():
    sock = socket.socket()
    try:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]
    finally:
        sock.close()


def _health(port, timeout=2.0):
    started = time.perf_counter()
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/health", timeout=timeout) as resp:
        body = resp.read()
        return resp.status, body, (time.perf_counter() - started) * 1000.0


def run_phone_once(pir, mode_dir, mock, run_index):
    agent = mock.agent_dir()
    run_dir = os.path.join(mode_dir, f"run{run_index:02d}")
    work = os.path.join(run_dir, "work")
    os.makedirs(work, exist_ok=True)
    port = _free_port()
    log_path = os.path.join(run_dir, "phone.pty.log")
    pty = None
    result = {"status": "N/A", "run": run_index,
              "command": f'"{pir}" -e builtin:phone interactive --no-session '
                         f'(then /phone-start {port})',
              "log": rel(log_path),
              "realDevice": "NOT MEASURABLE: no physical phone/browser client and no WiFi "
                            "transport is driven by this host; only the localhost HTTP server is measured."}
    try:
        pty, setup_ms, started, _env = _launch_tui(pir, agent, work, "-e builtin:phone", log_path)
        _byte_ms, frame_ms = _wait_first_frame(pty, started)
        if frame_ms is None:
            result["reason"] = "TUI did not paint a first frame; phone leg not started"
            return result
        time.sleep(1.0)
        status = None
        body = b""
        attempts = 0
        ready_ms = None
        for attempt in range(2):
            attempts = attempt + 1
            attempt_started = time.perf_counter()
            pty.write(f"/phone-start {port}".encode())
            time.sleep(0.25)
            pty.write(b"\r")
            while time.perf_counter() - attempt_started < 30:
                try:
                    status, body, _lat = _health(port)
                    ready_ms = (time.perf_counter() - attempt_started) * 1000.0
                    break
                except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
                    time.sleep(0.1)
            if status == 200:
                break
        if status != 200:
            result["reason"] = (f"/api/health never returned 200 after {attempts} attempt(s); "
                                f"TUI tail={bytes(pty.snapshot())[-400:]!r}")
            return result
        latencies = []
        for _ in range(10):
            try:
                _s, _b, lat = _health(port)
                latencies.append(lat)
            except Exception:
                break
        rss = big.sample_rss_mb(pty.pid)
        counters = process_counters(pty.pid)
        pty.write(b"/phone-stop\r")
        time.sleep(1.0)
        stopped = False
        try:
            _health(port, timeout=1.0)
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
            stopped = True
        result.update({
            "status": "measured",
            "port": port,
            "attempts": attempts,
            "startToHealthMs": round(ready_ms, 1),
            "healthLatenciesMs": [round(lat, 2) for lat in latencies],
            "healthLatencyMedianMs": round(statistics.median(latencies), 2) if latencies else None,
            "healthLatencyMinMs": round(min(latencies), 2) if latencies else None,
            "healthSamples": len(latencies),
            "healthBody": body[:200].decode("utf-8", errors="replace"),
            "tuiRssMb": round(rss, 1) if rss is not None else None,
            "tuiThreads": counters[2] if counters else None,
            "stopClosedPort": stopped,
        })
    except Exception as exc:
        result["reason"] = f"phone leg failed: {exc}"
    finally:
        if pty is not None:
            try:
                _quit_tui(pty)
            except Exception:
                pass
            try:
                write_bytes(log_path, bytes(pty.snapshot()))
            except Exception:
                pass
            try:
                pty.terminate()
            except Exception:
                pass
            try:
                pty.close()
            except Exception:
                pass
    return result


def run_phone(pir, mode_dir, mock, runs):
    runs_out = [run_phone_once(pir, mode_dir, mock, i) for i in range(1, runs + 1)]
    ok = [r for r in runs_out if r["status"] == "measured"]
    return {
        "status": "measured" if ok else "N/A",
        "reason": None if ok else "every phone leg failed",
        "runs": runs_out,
        "startToHealthMs": stats([r["startToHealthMs"] for r in ok]),
        "healthMedianMs": stats([r["healthLatencyMedianMs"] for r in ok]),
        "healthMinMs": stats([r["healthLatencyMinMs"] for r in ok]),
        "tuiRssMb": stats([r["tuiRssMb"] for r in ok]),
        "tuiThreads": stats([r["tuiThreads"] for r in ok]),
        "command": f'"{pir}" -e builtin:phone interactive --no-session (then /phone-start <port>)',
        "realDevice": "NOT MEASURABLE: no physical phone/browser client and no WiFi transport "
                      "is driven by this host; only the localhost HTTP server is measured.",
    }


# ---------------------------------------------------------------------------
# report
# ---------------------------------------------------------------------------

def _median_pair(stat):
    if not stat or stat.get("median") is None:
        return "N/A"
    return f"{stat['median']:.1f} / {stat['min']:.1f} / {stat['max']:.1f}"


def _evidence_label(build_kind, stale):
    if build_kind == "release":
        if stale and stale.get("stale"):
            return ("RELEASE BINARY — STALE vs source (still the user-designated evidence; "
                    "see the stale row)")
        return "FINAL EVIDENCE (release binary)"
    if build_kind == "debug":
        return "SMOKE TEST (debug binary, not final evidence)"
    return "UNKNOWN BUILD KIND — treat as smoke test"


def build_report(args, identity, stale, build_kind, results, started_at, elapsed_s):
    lines = []
    add = lines.append
    add(f"# Mode scorecard — pir `{identity.get('version') or '?'}`")
    add("")
    add(f"Generated `{started_at}` by `scripts/perf/mode_scorecard.py` "
        f"(wall {elapsed_s:.1f} s).")
    add("")
    add(f"> **{_evidence_label(build_kind, stale)}**")
    add("")
    add("## Binary under test")
    add("")
    add("| Field | Value |")
    add("|---|---|")
    add(f"| Path | `{identity.get('path')}` |")
    add(f"| Size | {identity.get('size')} B |")
    add(f"| mtime | `{identity.get('mtime')}` |")
    add(f"| SHA-256 | `{identity.get('sha256')}` |")
    add(f"| `--version` | `{identity.get('version')}` |")
    add(f"| Build kind | **{build_kind}** |")
    if stale:
        add(f"| Newest build input | `{stale.get('newestSource')}` at `{stale.get('newestSourceMtime')}` |")
        add(f"| Stale vs source | **{stale.get('stale')}** |")
    add("")
    add("## Summary")
    add("")
    add("Every leg defaults to 5 runs; `median / min / max` is over successful runs. "
        "RSS is the process-tree peak (root peak when the process is too short to walk "
        "the tree); threads are the root process's live threads.")
    add("")
    add("| Mode | Variant | Metric | median / min / max | n | RSS | threads | Status |")
    add("|---|---|---|---:|---:|---|---:|---|")

    def row(mode, variant, metric, stat, rss=None, threads=None, status="measured"):
        add(f"| {mode} | {variant} | {metric} | {_median_pair(stat)} | "
            f"{stat.get('n', 0) if stat else 0} | {rss or '—'} | {threads or '—'} | {status} |")

    cold = results.get("cold")
    if cold and cold.get("samples"):
        for label in ("version", "help"):
            data = cold["samples"][label]
            row("cold", f"--{label}", "wall ms", data["wall"],
                fmt_mb(data["peakRootMb"]["median"]), fmt_num(data["threadsPeak"]["median"], 0))
    lm = results.get("list-models")
    if lm and lm.get("variants"):
        for variant, data in lm["variants"].items():
            row("list-models", variant, "wall ms", data["wall"],
                fmt_mb((data["peakRootMb"] or {}).get("median")),
                fmt_num((data["threadsPeak"] or {}).get("median"), 0), data["status"])
    pr = results.get("print")
    if pr and pr.get("variants"):
        for variant, data in pr["variants"].items():
            row("print", variant, "wall ms", data["wall"],
                fmt_mb((data["peakTreeMb"] or {}).get("median")),
                fmt_num((data["threadsPeak"] or {}).get("median"), 0), data["status"])
    tools = results.get("tools")
    if tools and tools.get("tools"):
        for tool, data in tools["tools"].items():
            row("tools", tool, "wall ms", data["wall"],
                fmt_mb((data["peakTreeMb"] or {}).get("median")),
                fmt_num((data["threadsPeak"] or {}).get("median"), 0), data["status"])
    rpc = results.get("rpc-idle")
    if rpc and rpc.get("idlePeakMb"):
        row("rpc-idle", "idle", "tree RSS peak MB", rpc["idlePeakMb"],
            fmt_mb(rpc["idlePeakMb"]["median"]), fmt_num(rpc["threads"]["median"], 0))
        row("rpc-idle", "idle", "tree RSS last MB", rpc["idleLastMb"],
            fmt_mb(rpc["idleLastMb"]["median"]), fmt_num(rpc["threads"]["median"], 0))
        row("rpc-idle", "idle", "CPU % of one core", rpc["cpuPct"], "—", "—")
        row("rpc-idle", "hello", "round-trip ms", rpc["helloSteadyMs"], "—", "—")
        row("rpc-idle", "prompt", "prompt→agent_settled ms", rpc["promptSettledMs"], "—", "—")
    sv = results.get("server")
    if sv and sv.get("startupMs"):
        row("server", "start", "start→listening ms", sv["startupMs"],
            fmt_mb(sv["idlePeakMb"]["median"]), fmt_num(sv["threads"]["median"], 0), sv["status"])
        row("server", "idle", "tree RSS peak MB", sv["idlePeakMb"],
            fmt_mb(sv["idlePeakMb"]["median"]), fmt_num(sv["threads"]["median"], 0), sv["status"])
        row("server", "client", "round-trip ms", sv["clientMs"], "—", "—", sv["status"])
        row("server", "stop", "stop ms", sv["stopMs"], "—", "—", sv["status"])
    ev = results.get("env")
    if ev and ev.get("syncMs"):
        row("env", "startup", "sync line ms", ev["syncMs"],
            fmt_mb(ev["rssMb"]["median"]), fmt_num(ev["threads"]["median"], 0), ev["status"])
        row("env", "hello", "hello round-trip ms", ev["helloMs"], "—", "—", ev["status"])
        row("env", "file", "write+read+close total ms", ev["fileTotalMs"], "—", "—", ev["status"])
    tui = results.get("tui")
    if tui and tui.get("firstFrameMs"):
        row("tui", "first frame", "first frame ms", tui["firstFrameMs"],
            fmt_mb(tui["rssAtFirstFrameMb"]["median"]),
            fmt_num(tui["threadsAtFrame"]["median"], 0), tui["status"])
        row("tui", "first byte", "first byte ms", tui["firstByteMs"], "—", "—", tui["status"])
    ph = results.get("phone")
    if ph and ph.get("startToHealthMs"):
        row("phone", "start", "/phone-start→health 200 ms", ph["startToHealthMs"],
            fmt_mb(ph["tuiRssMb"]["median"]), fmt_num(ph["tuiThreads"]["median"], 0), ph["status"])
        row("phone", "health", "/api/health median ms", ph["healthMedianMs"],
            fmt_mb(ph["tuiRssMb"]["median"]), fmt_num(ph["tuiThreads"]["median"], 0), ph["status"])
    add("")

    add("## Details")
    add("")
    if cold and cold.get("samples"):
        add("### cold start")
        add("")
        add(f"Command: `pir --version` and `pir --help`, {args.cold_runs} fresh processes each "
            f"(spawn→exit; includes this harness's CreateProcess overhead). RSS is the root "
            f"process's OS-reported peak working set, read from a handle kept open past exit.")
        add("")
        for label in ("version", "help"):
            data = cold["samples"][label]
            add(f"- `--{label}`: median **{data['medianMs']:.2f} ms**, min {data['minMs']:.2f}, "
                f"max {data['maxMs']:.2f}; samples `{data['timesMs']}`; peak RSS median "
                f"{fmt_mb(data['peakRootMb']['median'])}, threads "
                f"{fmt_num(data['threadsPeak']['max'], 0) if data['threadsPeak']['max'] is not None else 'N/A (sub-10 ms leg)'}")
        add("")
    if lm and lm.get("variants"):
        add("### list-models")
        add("")
        add("`mock` uses the isolated mock agent dir; `openrouter` adds a builtin-provider entry "
            "to models.json (the fixture that exposes a duplicate catalog parse).")
        add("")
        for variant, data in lm["variants"].items():
            add(f"- {variant}: median **{data['wall']['median']:.2f} ms** "
                f"(min {data['wall']['min']:.2f}, max {data['wall']['max']:.2f}); "
                f"{data['stdoutLines']} stdout lines; peak RSS "
                f"{fmt_mb((data['peakRootMb'] or {}).get('median'))}")
        add("")
    if pr and pr.get("variants"):
        add("### print (mock provider)")
        add("")
        for variant, data in pr["variants"].items():
            add(f"- `{variant}`: median **{data['wall']['median']:.1f} ms** "
                f"(min {data['wall']['min']:.1f}, max {data['wall']['max']:.1f}); peak tree RSS "
                f"{fmt_mb(data['peakRssMb'])}; all OK="
                f"{all(r['stdoutHasOK'] for r in data['runs'])}")
        add("")
    if tools and tools.get("tools"):
        add("### tool execution (one read/bash call per print run)")
        add("")
        add("The mock answers request 1 with a single tool call and request 2 with `DONE`; "
            "`toolResultSeen` means the second request body contained the tool's output.")
        add("")
        for tool, data in tools["tools"].items():
            add(f"- `{tool}`: median **{data['wall']['median']:.1f} ms** "
                f"(min {data['wall']['min']:.1f}, max {data['wall']['max']:.1f}); peak tree RSS "
                f"{fmt_mb((data['peakTreeMb'] or {}).get('median'))}; "
                f"tool result seen={all(r['toolResultSeen'] for r in data['runs'])}, "
                f"DONE={all(r['stdoutHasDONE'] for r in data['runs'])}")
        add("")
    if rpc and rpc.get("idlePeakMb"):
        add("### idle RPC and JSON-lines round-trip")
        add("")
        add(f"Command: `{rpc['command']}`. Each of {args.rpc_runs} sessions is sampled for "
            f"{args.rpc_seconds:g} s (tree RSS, root CPU, threads), then 10 `hello` commands "
            f"and one `prompt` (`Reply with OK`) are sent; `prompt→agent_settled` is the full "
            f"turn including the mock provider.")
        add("")
        add(f"- idle tree RSS peak median **{fmt_mb(rpc['idlePeakMb']['median'])}**, last "
            f"**{fmt_mb(rpc['idleLastMb']['median'])}**; CPU "
            f"**{fmt_num(rpc['cpuPct']['median'], 2)}%** of one core; threads "
            f"**{fmt_num(rpc['threads']['median'], 0)}**")
        add(f"- hello steady-state median **{fmt_num(rpc['helloSteadyMs']['median'], 3)} ms** "
            f"(min {fmt_num(rpc['helloSteadyMs']['min'], 3)}, max "
            f"{fmt_num(rpc['helloSteadyMs']['max'], 3)}); first hello median "
            f"{fmt_num(rpc['firstHelloMs']['median'], 2)} ms")
        add(f"- prompt→agent_settled median **{fmt_num(rpc['promptSettledMs']['median'], 1)} ms** "
            f"(n={rpc['promptSettledMs']['n']})")
        add("")
    if sv and sv.get("startupMs"):
        add("### server start/idle/client/stop")
        add("")
        add(f"Command: `{sv['command']}`. Each run binds a fresh ephemeral loopback port; the "
            f"actual address is parsed from the `listening on` line.")
        add("")
        add(f"- start→listening median **{fmt_ms(sv['startupMs']['median'])}** "
            f"(min {fmt_ms(sv['startupMs']['min'])}, max {fmt_ms(sv['startupMs']['max'])}, "
            f"n={sv['startupMs']['n']})")
        add(f"- idle tree RSS peak median **{fmt_mb(sv['idlePeakMb']['median'])}**, last "
            f"{fmt_mb(sv['idleLastMb']['median'])}; CPU {fmt_num(sv['cpuPct']['median'], 2)}%; "
            f"threads {fmt_num(sv['threads']['median'], 0)}")
        add(f"- `pir client` round-trip median **{fmt_ms(sv['clientMs']['median'])}**; "
            f"stop median {fmt_ms(sv['stopMs']['median'])}")
        add("")
    if ev and ev.get("syncMs"):
        add("### env daemon round-trip")
        add("")
        add(f"Command: `{ev['command']}`. Frames are upstream protocol-v1 length-prefixed JSON.")
        add("")
        add(f"- sync line median **{fmt_num(ev['syncMs']['median'], 2)} ms**; hello median "
            f"**{fmt_num(ev['helloMs']['median'], 2)} ms**; file round-trip (write/lstat/open/"
            f"pread/close) median **{fmt_num(ev['fileTotalMs']['median'], 2)} ms**")
        add(f"- daemon RSS median {fmt_mb(ev['rssMb']['median'])}, threads "
            f"{fmt_num(ev['threads']['median'], 0)}")
        add("")
    if tui and tui.get("firstFrameMs"):
        add("### TUI first frame (ConPTY)")
        add("")
        add(f"Command: `{tui['command']}`. A frame is a complete synchronized-update block; "
            f"its end marker `ESC[?2026l` is the boundary `tui_session_probe.py` uses. Timing "
            f"starts before ConPTY/Pty creation, so it includes process creation and ConPTY setup.")
        add("")
        add(f"- first frame median **{fmt_num(tui['firstFrameMs']['median'], 1)} ms** "
            f"(min {fmt_num(tui['firstFrameMs']['min'], 1)}, max "
            f"{fmt_num(tui['firstFrameMs']['max'], 1)}, n={tui['firstFrameMs']['n']}); "
            f"first byte median {fmt_num(tui['firstByteMs']['median'], 1)} ms")
        add(f"- RSS at first frame median {fmt_mb(tui['rssAtFirstFrameMb']['median'])}, "
            f"threads {fmt_num(tui['threadsAtFrame']['median'], 0)}")
        for sample in tui["runs"]:
            add(f"  - run {sample['run']}: frame {fmt_ms(sample['firstFrameMs'])}, "
                f"setup {fmt_ms(sample['setupMs'])}, log `{sample['log']}`")
        add("")
    if ph and ph.get("startToHealthMs"):
        add("### phone (localhost only)")
        add("")
        add(f"Command: `{ph['command']}`. `/phone-start` is the builtin phone plugin's TUI "
            f"command; `-e builtin:phone` is the documented opt-in.")
        add("")
        add(f"- `/phone-start` → first `/api/health` 200 median **"
            f"{fmt_ms(ph['startToHealthMs']['median'])}** (n={ph['startToHealthMs']['n']})")
        add(f"- `/api/health` latency median **{fmt_num(ph['healthMedianMs']['median'], 2)} ms** "
            f"(median of per-run minimums {fmt_num(ph['healthMinMs']['median'], 2)} ms)")
        add(f"- TUI RSS median {fmt_mb(ph['tuiRssMb']['median'])}, threads "
            f"{fmt_num(ph['tuiThreads']['median'], 0)}")
        add(f"- {ph['realDevice']}")
        add("")

    add("## Reference data (not this run)")
    add("")
    add("| Source | Binary | `--version` | print | list-models | server start | TUI frame |")
    add("|---|---|---:|---:|---:|---:|---:|")
    old = REF_OLD_SCORECARD_DEBUG
    add(f"| old scorecard (debug, load-contaminated) | {old['binary']} | "
        f"{old['version_median_ms']} ms | {old['print_median_ms']} ms | — | "
        f"{old['server_startup_ms']} ms | {old['tui_first_frame_median_ms']} ms |")
    dbg = REF_CLEAN_DEBUG
    add(f"| clean debug (lane-cold-start) | {dbg['binary']} | {dbg['version_median_ms']} ms | "
        f"{dbg['print_median_ms']} ms | {dbg['list_models_openrouter_median_ms']} ms | "
        f"{dbg['server_startup_ms']} ms | {dbg['tui_first_frame_median_ms']} ms |")
    rel_old = REF_OLD_RELEASE
    add(f"| previous 0.0.1 release | {rel_old['binary']} | {rel_old['version_median_ms']} ms | "
        f"{rel_old['print_median_ms']} ms | {rel_old['list_models_median_ms']} ms | — | — |")
    add("")
    add("The old scorecard numbers were taken while other lanes were compiling "
        "(`--version` max 1488.7 ms, print 2800-4342 ms, TUI Pty setup 1.4-1.6 s); they are "
        "load/measurement noise, not product fixed cost. The clean debug lane is the same "
        "quiet-host convention as this run and is the right debug-vs-release anchor.")
    add("")

    add("## Fixed cost vs measurement/load noise")
    add("")
    add("- **Product fixed cost** visible in every release run: Windows image load + Rust "
        "runtime + clap (`--version`), clap help rendering (`--help`), settings/migrations, "
        "builtin catalog parse/registration (`list-models`, print startup, TUI loadConfig), "
        "provider HTTP-client construction before the first request, server bind/Axum setup, "
        "ConPTY terminal init, and env-daemon protocol setup. These scale with the binary, not "
        "with the host's parallel load.")
    add("- **Measurement/load noise**: Python `CreateProcess`+wait overhead (cold), Windows "
        "filesystem cache state (first-touch can be 20x), Defender scanning of a fresh binary, "
        "concurrent cargo lanes (the old scorecard's 1.5 s cold max and 3-4 s print), ConPTY "
        "setup in the first-frame number, the server harness's 20 ms line-poll granularity, "
        "and Windows working-set trimming during idle RSS windows.")
    add("- **Debug vs release**: the debug binary is ~312 MB vs 67 MB release, and CPU-bound "
        "serde/catalog/print paths are ~1.6-3x slower in debug. Compare like with like; the "
        "clean-debug column above is the anchor, not the load-contaminated old scorecard.")
    add("")

    add("## Residual / not measurable")
    add("")
    add("- **Phone real device/WebSocket**: no physical phone, WiFi/LAN transport or browser "
        "client is driven here; only the embedded localhost HTTP server is measured.")
    add("- **First-touch cold start** cannot be separated from Defender/page-in without a "
        "system-level cache drop or a Defender exclusion; both were left untouched.")
    add("- **Peak RSS on very short legs** is the OS-reported root-process peak working set; a "
        "sub-10 ms child tree cannot be walked reliably, so child spikes below the 10 ms "
        "sampling cadence are missed.")
    add("- **CPU on idle legs** is root-process CPU over the window; sub-1% values are "
        "dominated by sampler noise.")
    add("- **Working-set trimming**: a server idle peak can exceed the last value when Windows "
        "trims the working set mid-window; both are reported.")
    add("")
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pir", default=os.path.join(REPO, "target", "release", "pir.exe"),
                    help="pir.exe under test (default target/release/pir.exe)")
    ap.add_argument("--out", default=os.path.join(REPO, "logs", "lane-scorecard-final"))
    ap.add_argument("--cold-runs", type=int, default=5)
    ap.add_argument("--list-models-runs", type=int, default=5)
    ap.add_argument("--print-runs", type=int, default=5)
    ap.add_argument("--tools-runs", type=int, default=5)
    ap.add_argument("--rpc-runs", type=int, default=5)
    ap.add_argument("--server-runs", type=int, default=5)
    ap.add_argument("--env-runs", type=int, default=5)
    ap.add_argument("--tui-runs", type=int, default=5)
    ap.add_argument("--phone-runs", type=int, default=5)
    ap.add_argument("--rpc-seconds", type=float, default=5.0)
    ap.add_argument("--server-seconds", type=float, default=5.0)
    ap.add_argument("--only", default="", help="comma list: " + ",".join(MODES))
    ap.add_argument("--skip", default="", help="comma list: " + ",".join(MODES))
    ap.add_argument("--report-name", default="SCORECARD.md")
    args = ap.parse_args(argv)

    if os.name != "nt":
        print("ERROR: mode_scorecard.py is Windows-only (ConPTY + Toolhelp32)", file=sys.stderr)
        return 2
    if tui_drive is None:
        print(f"ERROR: cannot import scripts/tui_conpty/tui_drive.py: {TUI_IMPORT_ERROR}",
              file=sys.stderr)
        return 2
    pir = os.path.abspath(args.pir)
    if not os.path.isfile(pir):
        print(f"ERROR: pir not found: {pir}", file=sys.stderr)
        return 2

    selected = [m for m in MODES if m in (args.only.split(",") if args.only else MODES)]
    if args.skip:
        selected = [m for m in selected if m not in args.skip.split(",")]
    if not selected:
        print("ERROR: no mode selected", file=sys.stderr)
        return 2

    stamp = now_stamp()
    out_dir = os.path.abspath(args.out)
    raw_root = os.path.join(out_dir, f"raw-{stamp}")
    os.makedirs(raw_root, exist_ok=True)

    identity = big.binary_identity(pir)
    stale = big.staleness_note(pir)
    build_kind = detect_build_kind(pir, identity)
    print(f"[scorecard] binary {identity['path']} size={identity['size']} "
          f"mtime={identity['mtime']} version={identity['version']} kind={build_kind}")
    if stale and stale.get("stale"):
        print(f"[scorecard] STALE BINARY: {stale['newestSource']} is newer than the build — "
              f"the report will say so")

    mock = MockProvider()
    results = {}
    started_at = time.strftime("%Y-%m-%d %H:%M:%S")
    t_start = time.perf_counter()
    try:
        for mode in selected:
            mode_dir = os.path.join(raw_root, mode)
            os.makedirs(mode_dir, exist_ok=True)
            print(f"[scorecard] {mode} ...", flush=True)
            t0 = time.perf_counter()
            try:
                if mode == "cold":
                    samples, rows = run_cold(pir, mode_dir, args.cold_runs)
                    results[mode] = {"status": "measured", "samples": samples,
                                     "rows": rows, "rawDir": rel(mode_dir)}
                elif mode == "list-models":
                    results[mode] = run_list_models(pir, mode_dir, mock, args.list_models_runs)
                elif mode == "print":
                    results[mode] = run_print(pir, mode_dir, mock, args.print_runs)
                elif mode == "tools":
                    results[mode] = run_tools(pir, mode_dir, args.tools_runs)
                elif mode == "rpc-idle":
                    results[mode] = run_rpc_idle(pir, mode_dir, mock,
                                                 args.rpc_runs, args.rpc_seconds)
                elif mode == "server":
                    results[mode] = run_server(pir, mode_dir, args.server_seconds,
                                               args.server_runs)
                elif mode == "env":
                    results[mode] = run_env(pir, mode_dir, args.env_runs)
                elif mode == "tui":
                    results[mode] = run_tui(pir, mode_dir, mock, args.tui_runs)
                elif mode == "phone":
                    results[mode] = run_phone(pir, mode_dir, mock, args.phone_runs)
            except Exception as exc:
                results[mode] = {"status": "N/A", "reason": f"harness error: {exc}"}
            print(f"[scorecard] {mode}: {results[mode].get('status')} "
                  f"({time.perf_counter() - t0:.1f} s)", flush=True)
    finally:
        mock.close()

    elapsed = time.perf_counter() - t_start
    report = build_report(args, identity, stale, build_kind, results, started_at, elapsed)
    report_path = os.path.join(out_dir, args.report_name)
    write_text(report_path, report)
    write_text(os.path.join(out_dir, "scorecard.json"), json.dumps({
        "generated": started_at,
        "elapsedS": round(elapsed, 1),
        "pir": identity,
        "buildKind": build_kind,
        "stale": stale,
        "args": vars(args),
        "reference": {
            "oldScorecardDebug": REF_OLD_SCORECARD_DEBUG,
            "cleanDebug": REF_CLEAN_DEBUG,
            "oldRelease": REF_OLD_RELEASE,
        },
        "results": results,
    }, indent=2))
    print(f"[scorecard] wrote {report_path} and scorecard.json")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
