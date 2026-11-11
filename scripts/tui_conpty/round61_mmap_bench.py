# -*- coding: utf-8 -*-
"""Round61 mmap spill-preview benchmark runner.

Generates one 58 MB / 1,000,000-line spill file, then runs the rust bench in
`read` (read_to_string + Vec<&str> + tail copy) and `mmap` (MappedLines) modes
while sampling the process Working Set / Private Bytes via
GetProcessMemoryInfo (no psutil on this host).
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
BENCH = os.path.join(ROOT, ".play", "evidence", "lane-tui", "round61-mmap-spill",
                     "spill_preview_bench.exe")
OUT = os.path.join(ROOT, ".play", "evidence", "lane-tui", "round61-mmap-spill")


class PMC(ctypes.Structure):
    _fields_ = [
        ("cb", wt.DWORD),
        ("PageFaultCount", wt.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t),
        ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
        ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t),
        ("PeakPagefileUsage", ctypes.c_size_t),
        ("PrivateUsage", ctypes.c_size_t),
    ]


psapi = ctypes.WinDLL("psapi")
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)


def sample(pid):
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    h = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not h:
        return None
    try:
        pmc = PMC()
        pmc.cb = ctypes.sizeof(PMC)
        if not psapi.GetProcessMemoryInfo(h, ctypes.byref(pmc), pmc.cb):
            return None
        return {"ws_mb": pmc.WorkingSetSize / 1048576.0,
                "private_mb": pmc.PrivateUsage / 1048576.0,
                "peak_ws_mb": pmc.PeakWorkingSetSize / 1048576.0}
    finally:
        kernel32.CloseHandle(h)


def run(mode, path, loops):
    env = dict(os.environ, PIR_ALLOC_TRACE="1")
    proc = subprocess.Popen([BENCH, mode, path, str(loops)],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, env=env)
    samples = []

    def sampler():
        while proc.poll() is None:
            s = sample(proc.pid)
            if s:
                samples.append(s)
            time.sleep(0.005)

    t = threading.Thread(target=sampler)
    t.start()
    out, err = proc.communicate()
    t.join()
    if proc.returncode != 0:
        raise RuntimeError(f"{mode} failed: {err}")
    row = json.loads(out.strip().splitlines()[-1])
    if samples:
        row["ws_mb"] = max(s["ws_mb"] for s in samples)
        row["private_mb"] = max(s["private_mb"] for s in samples)
        row["peak_ws_mb"] = max(s["peak_ws_mb"] for s in samples)
        row["samples"] = len(samples)
    return row


def main():
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(tempfile.gettempdir(), "pir-round61-spill-58mb.txt")
    if not os.path.exists(path) or os.path.getsize(path) < 58_000_000:
        line = b"row payload 0123456789 abcdefghijklmnopqrstuvwxyz\n"
        with open(path, "wb") as f:
            for _ in range(1_000_000):
                f.write(line)
    results = []
    for mode, loops in (("read", 1), ("mmap", 5)):
        row = run(mode, path, loops)
        row["file_bytes"] = os.path.getsize(path)
        results.append(row)
        print(json.dumps(row), flush=True)
    with open(os.path.join(OUT, "bench_results.jsonl"), "w", encoding="utf-8") as f:
        for row in results:
            f.write(json.dumps(row) + "\n")
    os.remove(path)


if __name__ == "__main__":
    main()
