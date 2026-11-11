# -*- coding: utf-8 -*-
"""Round 58 lane-tui: TUI session RSS soak (PIR-TUI-018 round).

100 user turns, each answered with 5 real `bash echo` tool calls = 500 tool
executions in ONE TUI session — the same 500-call workload as the R42 baseline
(132.7 MB peak), plus 2x the prompts. RSS is sampled after boot, every 10
turns and by a 250 ms background sampler for the peak; wall/CPU are recorded.
The report also computes the post-warmup (last 80%) linear slope plus R² so a
real leak is distinguishable from allocator warmup (protocol v5).
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
ROOT = os.path.abspath(os.environ.get("PIR_RSS_ROOT") or os.path.join(REPO, ".play", "evidence", "lane-tui", "round59-tui019-variants", "default"))
PIR = os.path.abspath(os.environ.get("PIR_TUI_EXE") or os.path.join(REPO, "target-gnu", "debug", "pir.exe"))

TURNS = int(os.environ.get("PIR_RSS_TURNS", "100"))
CALLS_PER_TURN = int(os.environ.get("PIR_RSS_CALLS", "5"))
SAMPLE_EVERY = int(os.environ.get("PIR_RSS_SAMPLE_EVERY", "10"))


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


def slope_r2(samples):
    """Least-squares slope (MB/turn) and R² over the given samples."""
    pts = [(float(s["turn"]), float(s["rss_mb"])) for s in samples if s.get("rss_mb") is not None]
    n = len(pts)
    if n < 3:
        return None, None
    mean_x = sum(x for x, _ in pts) / n
    mean_y = sum(y for _, y in pts) / n
    sxx = sum((x - mean_x) ** 2 for x, _ in pts)
    sxy = sum((x - mean_x) * (y - mean_y) for x, y in pts)
    if sxx == 0:
        return None, None
    slope = sxy / sxx
    ss_tot = sum((y - mean_y) ** 2 for _, y in pts)
    ss_res = sum((y - (mean_y + slope * (x - mean_x))) ** 2 for x, y in pts)
    r2 = None if ss_tot == 0 else 1.0 - ss_res / ss_tot
    return round(slope, 3), (None if r2 is None else round(r2, 3))


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    os.makedirs(ROOT, exist_ok=True)
    work = os.path.join(ROOT, "work")
    os.makedirs(work, exist_ok=True)
    agent = os.path.join(ROOT, "agent")
    rb.seed_agent_dir(agent, True, None)

    script = []
    for turn in range(TURNS):
        if CALLS_PER_TURN > 0:
            calls = [{"name": "bash",
                      "arguments": {"command": f"echo R59-T{turn:03d}-C{c:02d}"}}
                     for c in range(CALLS_PER_TURN)]
            script.append({"tool_calls": calls})
        script.append({"text": f"TURN-{turn:03d}-DONE"})
    mock = MockServer(log_path=os.path.join(ROOT, "mock_llm.log"), script=script)
    rb.inject_mock_models(agent, mock.base_url)

    env = dict(BASE_ENV)
    env.update({"PIR_AGENT_DIR": agent, "PIR_NO_UPDATE_CHECK": "1",
                "RUST_LOG": "error", "PYTHONIOENCODING": "utf-8"})
    # Round61 PIR-TUI-019 allocator high-water (opt-in): the TUI appends one
    # allocation sample per prompt to a file in this run's evidence dir.
    if os.environ.get("PIR_ALLOC_TRACE"):
        env["PIR_ALLOC_TRACE"] = os.environ["PIR_ALLOC_TRACE"]
        env["PIR_ALLOC_TRACE_FILE"] = os.path.join(ROOT, "alloc_trace.jsonl")
    t0 = time.time()
    args = os.environ.get("PIR_RSS_ARGS", "")
    pty = Pty(f'"{PIR}" interactive {args}', work, env, cols=120, rows=40)
    pid = k32.GetProcessId(pty.hproc)
    sampler, sampler_fh = start_sampler(pid, os.path.join(ROOT, "process_samples.txt"))
    samples = []
    prompts = 0
    timed_out_turns = 0
    try:
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
            else:
                timed_out_turns += 1
            time.sleep(0.05)
            if (turn + 1) % SAMPLE_EVERY == 0:
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
    sample_path = os.path.join(ROOT, "process_samples.txt")
    if os.path.isfile(sample_path):
        for line in open(sample_path, encoding="utf-8", errors="replace"):
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
    valid = [s for s in samples if s.get("rss_mb") is not None]
    baseline = valid[0]["rss_mb"]
    final_sample = valid[-1]["rss_mb"]
    # Protocol v5: drop the first 20% (allocator warmup), then slope/R².
    warmup = max(1, int(round(len(valid) * 0.2)))
    steady = valid[warmup:]
    slope, r2 = slope_r2(steady)
    report = {
        "pir": PIR,
        "turns": TURNS,
        "calls_per_turn": CALLS_PER_TURN,
        "tool_calls": TURNS * CALLS_PER_TURN,
        "prompts_sent": prompts,
        "timed_out_turns": timed_out_turns,
        "wall_s": wall,
        "samples": samples,
        "peak": peak,
        "peak_sample_count": count,
        "baseline_rss_mb": baseline,
        "final_rss_mb": final_sample,
        "growth_mb": round(final_sample - baseline, 1),
        "steady_samples": steady,
        "steady_slope_mb_per_turn": slope,
        "steady_r2": r2,
        "session_dir_bytes": sum(
            os.path.getsize(os.path.join(dp, f))
            for dp, _, fs in os.walk(work) for f in fs if os.path.isfile(os.path.join(dp, f))
        ),
        "screen_tail": final.decode("utf-8", "replace")[-1500:],
        "mock_requests": sum(1 for _ in open(os.path.join(ROOT, "mock_llm.log"), encoding="utf-8")),
    }
    with open(os.path.join(ROOT, "report.json"), "w", encoding="utf-8") as f:
        json.dump(report, f, indent=2)
    print(json.dumps({k: report[k] for k in (
        "turns", "tool_calls", "prompts_sent", "timed_out_turns", "wall_s",
        "baseline_rss_mb", "final_rss_mb", "growth_mb", "peak", "peak_sample_count",
        "steady_slope_mb_per_turn", "steady_r2")}, ensure_ascii=False))
    print("samples:", [(s["turn"], s["rss_mb"], s.get("cpu_s")) for s in samples])
    return 0


if __name__ == "__main__":
    sys.exit(main())
