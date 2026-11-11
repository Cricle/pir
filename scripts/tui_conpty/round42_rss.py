# -*- coding: utf-8 -*-
"""Round 42 lane-tui Task B: long TUI session RSS sampling (500 tool calls).

50 user turns, each answered with 10 parallel `bash echo` tool calls, i.e.
500 real tool executions / assistant+tool entries in one TUI session.
Working set sampled after boot and every 10 turns (6 points) plus a 250 ms
background sampler for the peak; wall clock per phase is recorded.
"""
import ctypes
import json
import os
import shutil
import subprocess
import sys
import time
from ctypes import wintypes as wt

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from tui_drive import Pty, BASE_ENV  # noqa: E402
import run_battery as rb  # noqa: E402
from mock_llm import MockServer  # noqa: E402
from probe_turn_latency import start_sampler  # noqa: E402

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
k32.GetProcessId.argtypes = [wt.HANDLE]
k32.GetProcessId.restype = wt.DWORD

REPO = os.path.dirname(os.path.dirname(HERE))
ROOT = os.path.join(REPO, ".play", "evidence", "lane-tui", "round42-rss")
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))

TURNS = 50
CALLS_PER_TURN = 10


def rss_mb(pid):
    out = subprocess.run(
        ["powershell", "-NoProfile", "-Command",
         f"$p=Get-Process -Id {pid} -ErrorAction SilentlyContinue;"
         "if($p){'{0} {1}' -f $p.CPU,$p.WorkingSet64}"],
        capture_output=True, text=True, timeout=30)
    parts = out.stdout.strip().split()
    if len(parts) != 2:
        return None
    return {"cpu_s": float(parts[0]), "rss_mb": round(float(parts[1]) / (1024 * 1024), 1)}


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT, exist_ok=True)
    work = os.path.join(ROOT, "work")
    os.makedirs(work, exist_ok=True)
    agent = os.path.join(ROOT, "agent")
    rb.seed_agent_dir(agent, True, None)

    script = []
    for turn in range(TURNS):
        calls = [{"name": "bash",
                  "arguments": {"command": f"echo R42-T{turn:03d}-C{c:02d}"}}
                 for c in range(CALLS_PER_TURN)]
        script.append({"tool_calls": calls})
        script.append({"text": f"TURN-{turn:03d}-DONE"})
    mock = MockServer(log_path=os.path.join(ROOT, "mock_llm.log"), script=script)
    rb.inject_mock_models(agent, mock.base_url)

    env = dict(BASE_ENV)
    env.update({"PIR_AGENT_DIR": agent, "PIR_NO_UPDATE_CHECK": "1",
                "RUST_LOG": "error", "PYTHONIOENCODING": "utf-8"})
    t0 = time.time()
    pty = Pty(f'"{PIR}" interactive', work, env, cols=120, rows=40)
    pid = k32.GetProcessId(pty.hproc)
    sampler, sampler_fh = start_sampler(pid, os.path.join(ROOT, "process_samples.txt"))
    samples = []
    try:
        # boot
        last_len, quiet_since = 0, time.time()
        while time.time() - t0 < 30:
            with pty.log_lock:
                n = len(pty.log)
            if n != last_len:
                last_len, quiet_since = n, time.time()
            if n > 0 and time.time() - quiet_since >= 3.5:
                break
            time.sleep(0.05)
        samples.append({"turn": 0, "t_s": round(time.time() - t0, 2), **rss_mb(pid)})
        prompts = 0
        for turn in range(TURNS):
            pty.write(f"turn {turn:03d}\r".encode())
            prompts += 1
            deadline = time.time() + 30
            marker = f"TURN-{turn:03d}-DONE".encode()
            while time.time() < deadline:
                with pty.log_lock:
                    buf = bytes(pty.log)
                if marker in buf:
                    break
                time.sleep(0.05)
            time.sleep(0.08)
            if (turn + 1) % 10 == 0:
                samples.append({"turn": turn + 1, "t_s": round(time.time() - t0, 2),
                                **rss_mb(pid)})
        wall = round(time.time() - t0, 2)
        with pty.log_lock:
            final = bytes(pty.log)
        for payload in (b"/quit\r", b"/quit\r", b"\x03"):
            pty.write(payload)
            time.sleep(0.5)
            if not pty.alive:
                break
    finally:
        try:
            pty.terminate()
        except Exception:
            pass
        sampler.terminate()
        try:
            sampler.wait(timeout=10)
        except Exception:
            sampler.kill()
        sampler_fh.close()
        mock.stop()

    peak = {"rss_mb": 0.0, "cpu_s": 0.0}
    count = 0
    if os.path.isfile(os.path.join(ROOT, "process_samples.txt")):
        for line in open(os.path.join(ROOT, "process_samples.txt"), encoding="utf-8", errors="replace"):
            parts = line.split()
            if len(parts) == 3:
                try:
                    ws = float(parts[2]) / (1024 * 1024)
                    cpu = float(parts[1])
                except ValueError:
                    continue
                count += 1
                if ws > peak["rss_mb"]:
                    peak = {"rss_mb": round(ws, 1), "cpu_s": cpu}
    baseline = samples[0]["rss_mb"]
    final_sample = samples[-1]["rss_mb"]
    report = {
        "pir": PIR,
        "turns": TURNS,
        "tool_calls": TURNS * CALLS_PER_TURN,
        "prompts_sent": prompts,
        "wall_s": wall,
        "samples": samples,
        "peak": peak,
        "peak_sample_count": count,
        "baseline_rss_mb": baseline,
        "final_rss_mb": final_sample,
        "growth_mb": round(final_sample - baseline, 1),
        "screen_tail": final.decode("utf-8", "replace")[-2000:],
        "mock_requests": sum(1 for _ in open(os.path.join(ROOT, "mock_llm.log"), encoding="utf-8")),
    }
    with open(os.path.join(ROOT, "report.json"), "w", encoding="utf-8") as f:
        json.dump(report, f, indent=2)
    print(json.dumps({k: report[k] for k in (
        "turns", "tool_calls", "prompts_sent", "wall_s", "baseline_rss_mb",
        "final_rss_mb", "growth_mb", "peak", "peak_sample_count")}, ensure_ascii=False))
    print("samples:", [(s["turn"], s["rss_mb"], s.get("cpu_s")) for s in samples])
    return 0


if __name__ == "__main__":
    sys.exit(main())
