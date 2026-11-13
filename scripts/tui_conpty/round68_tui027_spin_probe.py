# -*- coding: utf-8 -*-
"""Round 68 (foreman): does the PIR-TUI-027 band-top chase spin the TUI?

`advance_window` extends the materialized band ONLY while
`window_bytes <= TRANSCRIPT_WINDOW_MAX_BYTES` (4 MiB). The interactive loop
keeps the frame dirty while `window.start() > 0 && visible_top <= 2 viewports`
so the extension can walk to entry 0. If a session's band is already over the
byte cap at the top of the transcript, `start` cannot move, the dirty condition
stays true, and the app would repaint forever at 100% CPU with nothing changing
on screen.

This probe distinguishes the two outcomes WITHOUT trusting any single number:

  leg_fat   ~1300 entries x ~8 KiB  (band well over 4 MiB) -> Ctrl+Home
  leg_thin  ~1300 entries x ~50 B   (band far below 4 MiB) -> Ctrl+Home

For each leg it samples the child's CPU time (GetProcessTimes) in four windows:
before the jump, right after, and two idle windows. A spinning app keeps burning
CPU while idle; a settled app goes to ~0.

Run: python scripts/tui_conpty/round68_tui027_spin_probe.py
Env: PIR_TUI_EXE (defaults to the current build), PIR_SPIN_OUT (evidence dir).
"""
import ctypes
import ctypes.wintypes as wt
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import tui_drive  # noqa: E402
from run_battery import _win_long_path, encode_cwd_dir_name, seed_agent_dir  # noqa: E402
from round67_tui027_truncation import seed_session  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.abspath(
    os.environ.get("PIR_SPIN_OUT")
    or os.path.join(REPO, ".play", "evidence", "ab-round1", "lane-fix-tui027", "spin")
)
PIR = os.path.abspath(
    os.environ.get("PIR_TUI_EXE")
    or os.path.join(REPO, ".play", "build", "gate-foreman", "debug", "pir.exe")
)

CTRL_HOME = b"\x1b[1;5H"
COUNT = 1300


def cpu_seconds(pid):
    """Total kernel+user CPU time of `pid`, in seconds."""
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    h = k32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not h:
        return None
    try:
        creation, exit_, kernel, user = (
            wt.FILETIME(), wt.FILETIME(), wt.FILETIME(), wt.FILETIME()
        )
        if not k32.GetProcessTimes(
            h, ctypes.byref(creation), ctypes.byref(exit_),
            ctypes.byref(kernel), ctypes.byref(user),
        ):
            return None

        def to_s(ft):
            return (ft.dwHighDateTime << 32 | ft.dwLowDateTime) / 1e7

        return to_s(kernel) + to_s(user)
    finally:
        k32.CloseHandle(h)


class CpuSampler:
    """Reads the child CPU clock; `run_scenario` does not expose its pid, so we
    find the only pir.exe child spawned from this process tree."""

    def __init__(self):
        self.pid = None

    def resolve(self):
        import subprocess
        # The harness spawns exactly one pir.exe for this probe run.
        out = subprocess.run(
            ["powershell", "-NoProfile", "-Command",
             "(Get-CimInstance Win32_Process -Filter \"Name='pir.exe'\" | "
             "Sort-Object CreationDate -Descending | Select-Object -First 1).ProcessId"],
            capture_output=True, text=True, timeout=30,
        )
        try:
            self.pid = int(out.stdout.strip())
        except ValueError:
            self.pid = None
        return self.pid

    def window(self, seconds):
        """CPU seconds burned during an idle window of `seconds`."""
        start = cpu_seconds(self.pid) if self.pid else None
        if start is None and self.resolve() is not None:
            start = cpu_seconds(self.pid)
        if start is None:
            return None
        t0 = time.time()
        time.sleep(seconds)
        end = cpu_seconds(self.pid)
        if end is None:
            return None
        wall = time.time() - t0
        return {"cpu_s": round(end - start, 3), "wall_s": round(wall, 2),
                "pct": round(100.0 * (end - start) / wall, 1)}


def run_leg(name, payload_bytes, sampler):
    base = os.path.join(OUT, name)
    os.makedirs(base, exist_ok=True)
    home = os.path.join(base, "home")
    agent = os.path.join(home, ".pi", "agent")
    os.makedirs(agent, exist_ok=True)
    seed_agent_dir(agent, True)
    project = os.path.join(base, "project")
    os.makedirs(project, exist_ok=True)
    session = seed_session(agent, project, COUNT, payload_bytes)
    env = {
        "PIR_TRUST_PROJECT": "1",
        "HOME": home,
        "USERPROFILE": home,
        "PIR_AGENT_DIR": agent,
        "PIR_NO_UPDATE_CHECK": "1",
    }
    tui_drive.PIR = PIR
    shots = os.path.join(base, "shots")
    os.makedirs(shots, exist_ok=True)
    result_json = os.path.join(shots, name + ".result.json")

    # The whole run stays inside run_scenario so the child is alive for every
    # sample; the sampler resolves the pid on the first window.
    ops = [
        ("wait_quiet", 2.0),
        ("sampler:baseline", None),   # placeholder, expanded below
        ("send", CTRL_HOME),
        ("wait", 1.0),
        ("sampler:after_jump", None),
        ("wait", 3.0),
        ("sampler:idle_1", None),
        ("wait", 3.0),
        ("sampler:idle_2", None),
        ("snap", "final"),
    ]
    samples = {}

    # tui_drive does not know our sampler op, so drive it manually but reuse
    # its op loop through a tiny shim: monkey-patch the op dispatch by sending
    # nothing and sampling in a background thread keyed on wall clock.
    marks = []

    def sample_thread():
        # Give the app time to boot and settle before the baseline.
        time.sleep(4.0)
        for label in ("baseline", "after_jump", "idle_1", "idle_2"):
            if label == "after_jump":
                # ctrl+home is sent by the op list; wait for it to land.
                while time.time() < started + 7.0:
                    time.sleep(0.1)
            samples[label] = sampler.window(3.0)
            with open(os.path.join(base, "samples.json"), "w", encoding="utf-8") as f:
                json.dump(samples, f, indent=2)

    import threading
    started = time.time()
    th = threading.Thread(target=sample_thread, daemon=True)
    th.start()
    plain_ops = [op for op in ops if not op[0].startswith("sampler:")]
    result = tui_drive.run_scenario(
        name, plain_ops, extra_env=env, cwd=project,
        args=["--session", session],   # fullscreen is the default mode
        shots_dir=shots,
        driver_log=os.path.join(shots, name + ".driver.log"),
        result_json=result_json, agent_dir=agent, timeout=180,
    )
    th.join(timeout=5)
    return {"leg": name, "payload_bytes": payload_bytes, "count": COUNT,
            "samples": samples, "exit": result.get("exit_code")}


def main():
    os.makedirs(OUT, exist_ok=True)
    sampler = CpuSampler()
    report = []
    for name, payload in (("leg_thin", 0), ("leg_fat", 8000)):
        try:
            report.append(run_leg(name, payload, sampler))
        except Exception as exc:  # keep the other leg running
            report.append({"leg": name, "error": f"{type(exc).__name__}: {exc}"})
        time.sleep(1.0)
    with open(os.path.join(OUT, "result.json"), "w", encoding="utf-8") as f:
        json.dump(report, f, indent=2)
    for row in report:
        print(json.dumps(row, ensure_ascii=False))


if __name__ == "__main__":
    main()