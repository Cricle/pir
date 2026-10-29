# -*- coding: utf-8 -*-
"""Probe (foreman, 2026-10-16): what exactly happens after a single `/quit`?

Spawns the TUI in a ConPTY with a seeded(mock) agent dir, waits for boot, sends
ONE `/quit\\r`, then samples for 25 s:
  * process_alive  — kernel32.WaitForSingleObject(child, 0) != WAIT_OBJECT_0
  * pty_open       — the driver's read loop still sees the pipe open
  * bytes          — the pty's accumulated output size
so the two hypotheses are separable:
  H1 the process really lingers   -> process_alive stays True
  H2 the process exited but a grandchild holds the pty handles -> process_alive
     goes False while pty_open stays True.

Usage: python probe_quit_exit.py [--pir PATH] [--out DIR]
"""
import argparse, ctypes, os, shutil, subprocess, sys, time
from ctypes import wintypes as wt

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from tui_drive import Pty, BASE_ENV  # noqa: E402
import run_battery as rb  # noqa: E402

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
k32.GetProcessId.argtypes = [wt.HANDLE]
k32.GetProcessId.restype = wt.DWORD
k32.WaitForSingleObject.argtypes = [wt.HANDLE, wt.DWORD]
k32.WaitForSingleObject.restype = wt.DWORD


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pir", default=None)
    ap.add_argument("--out", default=None)
    ap.add_argument("--seconds", type=float, default=25.0)
    ap.add_argument("--no-model", action="store_true",
                    help="empty agent dir: reproduce the no-model boot")
    args = ap.parse_args()

    pir = args.pir or os.environ.get("PIR_TUI_EXE") or os.path.join(
        os.path.dirname(HERE), "..", "target-alt", "debug", "pir.exe")
    pir = os.path.abspath(pir)
    out = os.path.abspath(args.out or os.path.join(HERE, "out", "probe_quit_exit"))
    os.makedirs(out, exist_ok=True)
    agent = os.path.join(out, "agent")
    work = os.path.join(out, "work")
    os.makedirs(work, exist_ok=True)
    # Seed the same way the battery does: models.json/settings.json + the mock.
    mock = None
    if not args.no_model:
        rb.seed_agent_dir(agent, True)
        mock = rb.MockServer(log_path=os.path.join(out, "mock.log"))
        rb.inject_mock_models(agent, mock.base_url)

    env = dict(BASE_ENV)
    env.update({"PIR_AGENT_DIR": agent, "PIR_NO_UPDATE_CHECK": "1", "RUST_LOG": "error"})
    pty = Pty(f'"{pir}" interactive', work, env, cols=120, rows=40)
    pid = k32.GetProcessId(pty.hproc)
    print(f"[probe] pid={pid}")
    try:
        # Boot: quiet for 3.5 s.
        last, quiet_since = 0, time.time()
        while time.time() - quiet_since < 3.5:
            with pty.log_lock:
                n = len(pty.log)
            if n != last:
                last, quiet_since = n, time.time()
            time.sleep(0.05)
        with pty.log_lock:
            before = len(pty.log)
        print(f"[probe] booted ({before} bytes); sending ONE /quit")
        pty.write(b"/quit\r")
        t0 = time.time()
        exited_at = None
        while time.time() - t0 < args.seconds:
            alive = k32.WaitForSingleObject(pty.hproc, 0) != 0
            with pty.log_lock:
                n = len(pty.log)
            print(
                f"[probe] +{time.time()-t0:5.2f}s alive={int(alive)} pty={int(pty.alive)} bytes={n}"
            )
            if not alive and exited_at is None:
                exited_at = time.time() - t0
            if not alive and not pty.alive:
                break
            time.sleep(0.5)
        print(f"[probe] exited_at={exited_at} pty_alive={pty.alive}")
    finally:
        try:
            pty.terminate()
        except Exception:
            pass
        if mock:
            mock.stop()


if __name__ == "__main__":
    main()
