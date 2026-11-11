# -*- coding: utf-8 -*-
"""Run the ignored spill_preview 58MB bench test binary directly and sample
Working Set / Private Bytes through GetProcessMemoryInfo (no psutil)."""
import ctypes
import ctypes.wintypes as wt
import json
import os
import subprocess
import sys
import threading
import time

TEST = sys.argv[1]
OUT = os.path.dirname(os.path.abspath(TEST))


class PMC(ctypes.Structure):
    _fields_ = [
        ("cb", wt.DWORD), ("PageFaultCount", wt.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t), ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t), ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t),
        ("PrivateUsage", ctypes.c_size_t),
    ]


psapi = ctypes.WinDLL("psapi")
k32 = ctypes.WinDLL("kernel32", use_last_error=True)


def sample(pid):
    h = k32.OpenProcess(0x1000, False, pid)
    if not h:
        return None
    try:
        pmc = PMC()
        pmc.cb = ctypes.sizeof(PMC)
        if not psapi.GetProcessMemoryInfo(h, ctypes.byref(pmc), pmc.cb):
            return None
        return (pmc.WorkingSetSize / 1048576.0, pmc.PrivateUsage / 1048576.0)
    finally:
        k32.CloseHandle(h)


def main():
    env = dict(os.environ, PIR_ALLOC_TRACE="1")
    proc = subprocess.Popen(
        [TEST, "--exact", "tui::spill_preview::tests::bench_58mb", "--ignored", "--nocapture"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
    samples = []

    def sampler():
        while proc.poll() is None:
            s = sample(proc.pid)
            if s:
                samples.append(s)
            time.sleep(0.002)

    t = threading.Thread(target=sampler)
    t.start()
    out, err = proc.communicate()
    t.join()
    print(out)
    if err.strip():
        print("STDERR:", err[-2000:], file=sys.stderr)
    peak_ws = max((s[0] for s in samples), default=0)
    peak_priv = max((s[1] for s in samples), default=0)
    print(json.dumps({"peak_ws_mb": round(peak_ws, 1), "peak_private_mb": round(peak_priv, 1),
                      "samples": len(samples), "exit": proc.returncode}))
    with open(os.path.join(OUT, "bench_test_run.log"), "w", encoding="utf-8") as f:
        f.write(out + "\n" + json.dumps({"peak_ws_mb": peak_ws, "peak_private_mb": peak_priv,
                                         "samples": len(samples), "exit": proc.returncode}) + "\n")


if __name__ == "__main__":
    main()
