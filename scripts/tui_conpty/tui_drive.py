# Adapted from target/tui_shots/tui_drive.py (copied 2026-10-15) into
# scripts/tui_conpty/ as the repo-managed ConPTY acceptance driver.
# Changes vs the original: PIR now defaults to <repo>/target-alt/debug/pir.exe
# and honors the PIR_TUI_EXE env override; the snap/final-log dir (SHOTS), the
# child agent dir (PIR_AGENT_DIR) and the child cwd now resolve under
# scripts/tui_conpty/out/ and are created at run time.
# Battery changes (run_battery.py, 2026-10-15):
#   * ("log", ...) ops go to a SEPARATE driver log (<name>.driver.log), never
#     into the captured app stream, so rendered pyte screens contain app
#     output only (the old `<<<MARK ...>>>` writes polluted every snap).
#   * every op is traced in the driver log (send/wait/snap/mark with stamps).
#   * optional per-scenario wall-clock `timeout` (enforced between ops; drives
#     the graceful-quit path instead of hanging).
#   * --shots-dir / --driver-log / --agent-dir / --result-json so the battery
#     can give every run + scenario its own artifact directory, plus a
#     machine-readable result (exit code, restore-sequence counts, marks,
#     snaps, timeout flag).

"""ConPTY driver for pir TUI interactive acceptance runs.

Usage: python tui_drive.py <scenario.py> [--shots-dir DIR] [--driver-log FILE]
       [--agent-dir DIR] [--result-json FILE] [--timeout SECS] [--offline]
The scenario file defines OPS: a list of ops:
  ("send", b"bytes")            write bytes to the pty input
  ("wait", seconds)             sleep
  ("snap", "name")              flush current log to <shots>/<name>.log
  ("log", "text")               append a MARK to the DRIVER log only
  ("wait_quiet", seconds)       wait until output has been idle for N seconds (max cap)
At end: sends /quit, waits for exit, reports exit code + terminal restore markers.
"""
import ctypes, ctypes.wintypes as wt, os, sys, time, threading, subprocess, json

CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def kill_process_tree(pid):
    """Kill `pid` AND its descendants (`taskkill /T /F`), best effort.

    `TerminateProcess` reaches only the direct ConPTY child (pir.exe); tool
    subprocesses the app spawned (bash, node, a dev server) survive and keep
    the work dir busy, distorting later RSS legs. `pid` must belong to a
    process THIS harness spawned. Returns True when taskkill succeeded.
    """
    if not pid:
        return False
    try:
        result = subprocess.run(
            ["taskkill", "/T", "/F", "/PID", str(pid)],
            capture_output=True, timeout=15, creationflags=CREATE_NO_WINDOW,
        )
        return result.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def clipset(text, shots):
    tmp = os.path.join(shots, "_clip.txt")
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(text)
    subprocess.run(["powershell", "-NoProfile", "-Command",
                    "Set-Clipboard -Value (Get-Content -Raw -Encoding UTF8 '%s')" % tmp], capture_output=True)

def clipget():
    r = subprocess.run(["powershell", "-NoProfile", "-Command", "Get-Clipboard -Raw"],
                       capture_output=True)
    return r.stdout.decode("utf-8", errors="replace")


kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

# --- constants ---
PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE = 0x00020016
EXTENDED_STARTUPINFO_PRESENT = 0x00080000
CREATE_UNICODE_ENVIRONMENT = 0x00000400
INFINITE = 0xFFFFFFFF
STARTF_USESTDHANDLES = 0x00000100
WAIT_TIMEOUT = 0x102

class COORD(ctypes.Structure):
    _fields_ = [("X", ctypes.c_short), ("Y", ctypes.c_short)]

class STARTUPINFO(ctypes.Structure):
    _fields_ = [("cb", wt.DWORD), ("lpReserved", wt.LPWSTR),
                ("lpDesktop", wt.LPWSTR), ("lpTitle", wt.LPWSTR),
                ("dwX", wt.DWORD), ("dwY", wt.DWORD),
                ("dwXSize", wt.DWORD), ("dwYSize", wt.DWORD),
                ("dwXCountChars", wt.DWORD), ("dwYCountChars", wt.DWORD),
                ("dwFillAttribute", wt.DWORD), ("dwFlags", wt.DWORD),
                ("wShowWindow", wt.WORD), ("cbReserved2", wt.WORD),
                ("lpReserved2", ctypes.c_void_p),
                ("hStdInput", wt.HANDLE), ("hStdOutput", wt.HANDLE),
                ("hStdError", wt.HANDLE)]

class STARTUPINFOEX(ctypes.Structure):
    _fields_ = [("StartupInfo", STARTUPINFO), ("lpAttributeList", ctypes.c_void_p)]

class SECURITY_ATTRIBUTES(ctypes.Structure):
    _fields_ = [("nLength", wt.DWORD), ("lpSecurityDescriptor", ctypes.c_void_p),
                ("bInheritHandle", wt.BOOL)]

class PROCESS_INFORMATION(ctypes.Structure):
    _fields_ = [("hProcess", wt.HANDLE), ("hThread", wt.HANDLE),
                ("dwProcessId", wt.DWORD), ("dwThreadId", wt.DWORD)]

kernel32.CreatePseudoConsole.argtypes = [COORD, wt.HANDLE, wt.HANDLE, wt.DWORD, ctypes.POINTER(ctypes.c_void_p)]
kernel32.CreatePseudoConsole.restype = ctypes.HRESULT
kernel32.ClosePseudoConsole.argtypes = [ctypes.c_void_p]
kernel32.InitializeProcThreadAttributeList.restype = wt.BOOL
kernel32.InitializeProcThreadAttributeList.argtypes = [ctypes.c_void_p, wt.DWORD, wt.DWORD, ctypes.POINTER(ctypes.c_size_t)]
kernel32.UpdateProcThreadAttribute.restype = wt.BOOL
kernel32.UpdateProcThreadAttribute.argtypes = [ctypes.c_void_p, wt.DWORD, wt.DWORD, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_void_p]
kernel32.CreateProcessW.restype = wt.BOOL
kernel32.CreateProcessW.argtypes = [wt.LPCWSTR, wt.LPWSTR, ctypes.c_void_p, ctypes.c_void_p, wt.BOOL, wt.DWORD, ctypes.c_void_p, wt.LPCWSTR, ctypes.c_void_p, ctypes.c_void_p]
kernel32.ReadFile.restype = wt.BOOL
kernel32.ReadFile.argtypes = [wt.HANDLE, ctypes.c_void_p, wt.DWORD, ctypes.POINTER(wt.DWORD), ctypes.c_void_p]
kernel32.WriteFile.restype = wt.BOOL
kernel32.WriteFile.argtypes = [wt.HANDLE, ctypes.c_void_p, wt.DWORD, ctypes.POINTER(wt.DWORD), ctypes.c_void_p]
kernel32.GetExitCodeProcess.restype = wt.BOOL
kernel32.GetExitCodeProcess.argtypes = [wt.HANDLE, ctypes.POINTER(wt.DWORD)]
kernel32.WaitForSingleObject.restype = wt.DWORD
kernel32.WaitForSingleObject.argtypes = [wt.HANDLE, wt.DWORD]
kernel32.TerminateProcess.restype = wt.BOOL
kernel32.TerminateProcess.argtypes = [wt.HANDLE, wt.UINT]
kernel32.CloseHandle.restype = wt.BOOL
kernel32.CloseHandle.argtypes = [wt.HANDLE]

kernel32.ResizePseudoConsole.restype = ctypes.HRESULT
kernel32.ResizePseudoConsole.argtypes = [ctypes.c_void_p, COORD]

def make_pipe():
    r = wt.HANDLE(); w = wt.HANDLE()
    sa = SECURITY_ATTRIBUTES(); sa.nLength = ctypes.sizeof(sa); sa.bInheritHandle = True
    sa.lpSecurityDescriptor = None
    if not kernel32.CreatePipe(ctypes.byref(r), ctypes.byref(w), ctypes.byref(sa), 0):
        raise OSError("CreatePipe failed")
    return r, w

class Pty:
    def __init__(self, cmdline, cwd, env, cols=120, rows=40):
        size = COORD(cols, rows)
        in_r, in_w = make_pipe()
        out_r, out_w = make_pipe()
        self.in_w, self.out_r = in_w, out_r
        self.hpc = ctypes.c_void_p()
        hr = kernel32.CreatePseudoConsole(size, in_r, out_w, 0, ctypes.byref(self.hpc))
        if hr != 0:
            raise OSError(f"CreatePseudoConsole hr=0x{hr & 0xFFFFFFFF:08x}")
        kernel32.CloseHandle(in_r); kernel32.CloseHandle(out_w)
        attr_size = ctypes.c_size_t(0)
        kernel32.InitializeProcThreadAttributeList(None, 1, 0, ctypes.byref(attr_size))
        attr = ctypes.create_string_buffer(attr_size.value)
        if not kernel32.InitializeProcThreadAttributeList(attr, 1, 0, ctypes.byref(attr_size)):
            raise OSError("InitializeProcThreadAttributeList failed")
        if not kernel32.UpdateProcThreadAttribute(attr, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                self.hpc, ctypes.sizeof(ctypes.c_void_p), None, None):
            raise OSError("UpdateProcThreadAttribute failed")
        si = STARTUPINFOEX()
        si.StartupInfo.cb = ctypes.sizeof(si)
        # Prevent the child from inheriting the parent's (pipe) std handles:
        # with ConPTY the CRT std handles must be detached so console apps
        # fall back to the pseudoconsole (same trick as wezterm portable-pty).
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES
        INVALID = wt.HANDLE(-1).value or ctypes.c_void_p(-1)
        si.StartupInfo.hStdInput = INVALID
        si.StartupInfo.hStdOutput = INVALID
        si.StartupInfo.hStdError = INVALID
        si.lpAttributeList = ctypes.cast(attr, ctypes.c_void_p)
        pi = PROCESS_INFORMATION()
        env_block = None
        if env:
            parts = []
            for k, v in env.items():
                parts.append(f"{k}={v}")
            env_block = ctypes.create_unicode_buffer("\0".join(parts) + "\0\0")
        flags = EXTENDED_STARTUPINFO_PRESENT
        if env_block: flags |= CREATE_UNICODE_ENVIRONMENT
        ok = kernel32.CreateProcessW(None, ctypes.create_unicode_buffer(cmdline), None, None, False,
                flags, ctypes.cast(env_block, ctypes.c_void_p) if env_block else None,
                cwd, ctypes.byref(si), ctypes.byref(pi))
        if not ok:
            raise OSError(f"CreateProcess failed err={ctypes.get_last_error()}")
        self.hproc = pi.hProcess
        # Kept for the memory probes: the child's pid is what a process-tree
        # working-set sample starts from (`scripts/perf/tui_session_probe.py`).
        self.pid = pi.dwProcessId
        kernel32.CloseHandle(pi.hThread)
        self.log = bytearray()
        self.log_lock = threading.Lock()
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.alive = True
        self.reader.start()

    def _read_loop(self):
        buf = ctypes.create_string_buffer(65536)
        got = wt.DWORD(0)
        while True:
            ok = kernel32.ReadFile(self.out_r, buf, 65536, ctypes.byref(got), None)
            if not ok or got.value == 0:
                self.alive = False
                return
            with self.log_lock:
                self.log.extend(buf.raw[:got.value])

    def write(self, data: bytes):
        wrote = wt.DWORD(0)
        view = ctypes.create_string_buffer(data)
        ok = kernel32.WriteFile(self.in_w, view, len(data), ctypes.byref(wrote), None)
        if not ok:
            raise OSError("WriteFile to pty failed")

    def exit_code(self):
        code = wt.DWORD(0)
        kernel32.GetExitCodeProcess(self.hproc, ctypes.byref(code))
        return code.value

    def wait(self, timeout_s):
        return kernel32.WaitForSingleObject(self.hproc, int(timeout_s * 1000)) == 0

    def running(self):
        """True while the attached process is alive (handle-based, not pipe
        EOF: a ConPTY keeps its output pipe open after the app exits). This
        also guards `kill_process_tree` against PID reuse — taskkill is only
        run while our own process is still running."""
        return kernel32.WaitForSingleObject(self.hproc, 0) == WAIT_TIMEOUT

    def terminate(self):
        # Kill the whole tree while the root is alive: taskkill /T follows the
        # Windows parent links, and TerminateProcess below destroys them.
        if self.running():
            kill_process_tree(self.pid)
        kernel32.TerminateProcess(self.hproc, 1)

    def close(self):
        # A graceful `/quit` can still leave tool subprocesses behind; sweep
        # the tree while the root is verifiably alive, then release handles.
        if self.running():
            kill_process_tree(self.pid)
        kernel32.ClosePseudoConsole(self.hpc)
        kernel32.CloseHandle(self.in_w)
        kernel32.CloseHandle(self.out_r)
        kernel32.CloseHandle(self.hproc)

    def snapshot(self):
        with self.log_lock:
            return bytes(self.log)

# --- adapted paths (repo-managed copy; see file header) ---
HERE = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(HERE, "out")
AGENT_DIR = os.path.join(OUT, "tui_agent")

BASE_ENV = {
    "PIR_AGENT_DIR": AGENT_DIR,
    "PIR_NO_UPDATE_CHECK": "1",
    "RUST_LOG": "error",
    "SystemRoot": os.environ.get("SystemRoot", r"C:\Windows"),
    "TEMP": os.environ.get("TEMP", r"C:\Windows\Temp"),
    "TMP": os.environ.get("TMP", r"C:\Windows\Temp"),
    "USERPROFILE": os.environ.get("USERPROFILE", ""),
    "HOMEDRIVE": os.environ.get("HOMEDRIVE", "C:"),
    "HOMEPATH": os.environ.get("HOMEPATH", ""),
    "COMSPEC": os.environ.get("COMSPEC", r"C:\Windows\System32\cmd.exe"),
    "PATH": os.environ.get("PATH", ""),
    "NUMBER_OF_PROCESSORS": os.environ.get("NUMBER_OF_PROCESSORS", "8"),
    "OS": "Windows_NT",
    "PROCESSOR_ARCHITECTURE": os.environ.get("PROCESSOR_ARCHITECTURE", "AMD64"),
    "USERNAME": os.environ.get("USERNAME", "user"),
    "COMPUTERNAME": os.environ.get("COMPUTERNAME", "pc"),
    "LOCALAPPDATA": os.environ.get("LOCALAPPDATA", ""),
    "APPDATA": os.environ.get("APPDATA", ""),
    "PROGRAMDATA": os.environ.get("PROGRAMDATA", ""),
    "windir": os.environ.get("windir", r"C:\Windows"),
}

SHOTS = OUT  # adapted: snapshots/logs under scripts/tui_conpty/out/
PIR = os.environ.get("PIR_TUI_EXE") or os.path.join(REPO_ROOT, "target-alt", "debug", "pir.exe")

# Terminal restore sequences whose presence/counts in the FINAL log are the
# acceptance evidence (README "What PASS looks like").
RESTORE_SEQUENCES = {
    "1049l": b"\x1b[?1049l",   # leave alternate screen
    "1049h": b"\x1b[?1049h",   # enter alternate screen (context)
    "2004l": b"\x1b[?2004l",   # bracketed paste off
    "2004h": b"\x1b[?2004h",   # bracketed paste on (context)
    "25h": b"\x1b[?25h",       # cursor show
    "25l": b"\x1b[?25l",       # cursor hide (context)
    "1000l": b"\x1b[?1000l",   # mouse off (context)
    "1000h": b"\x1b[?1000h",   # mouse on (context)
    "1006l": b"\x1b[?1006l",   # SGR mouse off (context)
    "1006h": b"\x1b[?1006h",   # SGR mouse on (context)
}


def count_restore(data):
    return {key: data.count(seq) for key, seq in RESTORE_SEQUENCES.items()}


def run_scenario(name, ops, extra_env=None, args=None, cwd=None,
                 shots_dir=None, driver_log=None, result_json=None,
                 agent_dir=None, timeout=None):
    """Run one scenario. All app output goes to <shots>/<name>.log + <snap>.log;
    driver annotations/traces go to <driver_log> (default <shots>/<name>.driver.log)
    and a machine-readable summary to <result_json> (optional).

    `timeout` (seconds, wall clock) is enforced between ops: once exceeded the
    op loop stops, a TIMEOUT mark is recorded and the normal graceful-quit path
    runs (so a frozen app can never hang the harness)."""
    shots = shots_dir or SHOTS
    os.makedirs(shots, exist_ok=True)
    driver_log_path = driver_log or os.path.join(shots, name + ".driver.log")
    t0 = time.time()
    dlog_fh = open(driver_log_path, "w", encoding="utf-8", buffering=1)

    def dlog(msg):
        dlog_fh.write(f"[{time.time() - t0:8.2f}s] {msg}\n")

    deadline = (t0 + timeout) if timeout else None

    def over_budget():
        return deadline is not None and time.time() > deadline

    env = dict(BASE_ENV)
    if agent_dir:
        env["PIR_AGENT_DIR"] = agent_dir
    if extra_env:
        env.update(extra_env)
    target_cwd = cwd or agent_dir or AGENT_DIR
    os.makedirs(target_cwd, exist_ok=True)
    cmdline = f'"{PIR}" interactive'
    if args: cmdline += " " + " ".join(f'"{a}"' if " " in a else a for a in args)
    dlog(f"scenario={name} timeout={timeout} pir={PIR}")
    dlog(f"cwd={target_cwd} agent_dir={env.get('PIR_AGENT_DIR')} cmdline={cmdline}")

    pty = Pty(cmdline, target_cwd, env)
    marks = []
    snaps = []
    timeout_hit = False
    exit_code = None
    final_path = os.path.join(shots, name + ".log")
    try:
        for index, op in enumerate(ops):
            kind = op[0]
            if over_budget():
                timeout_hit = True
                marks.append(f"TIMEOUT: {timeout}s buffer exceeded before op #{index} ({kind})")
                dlog(marks[-1])
                break
            if kind == "send":
                pty.write(op[1])
                dlog(f"op#{index} send {len(op[1])}B {op[1][:48]!r}")
            elif kind == "wait":
                secs = op[1]
                if deadline is not None:
                    secs = min(secs, max(0.0, deadline - time.time()))
                time.sleep(secs)
                dlog(f"op#{index} wait {secs:.2f}s")
            elif kind == "wait_quiet":
                quiet = op[1]
                # The quiet window is measured from the last output, but the
                # WHOLE wait is bounded by an absolute cap: an app that keeps
                # redrawing (a stuck spinner, a busy loop) resets the quiet
                # timer forever and would otherwise run until the scenario's
                # hard timeout with no diagnostic.
                cap = quiet * 10 + 60
                if deadline is not None:
                    cap = min(cap, max(0.0, deadline - time.time()))
                op_deadline = time.time() + cap
                last = len(pty.snapshot())
                last_change = time.time()
                ended_idle = False
                while True:
                    now = time.time()
                    if now >= op_deadline:
                        break
                    time.sleep(min(0.2, max(0.0, op_deadline - now)))
                    n = len(pty.snapshot())
                    now = time.time()
                    if n != last:
                        last = n
                        last_change = now
                    elif now - last_change >= quiet:
                        ended_idle = True
                        break
                dlog(f"op#{index} wait_quiet {quiet}s -> "
                     f"{'idle' if ended_idle else 'at-cap'} after {time.time()-t0:.2f}s total")
            elif kind == "snap":
                data = pty.snapshot()
                path = os.path.join(shots, op[1] + ".log")
                with open(path, "wb") as f:
                    f.write(data)
                snaps.append({"name": op[1], "path": path,
                              "at_s": round(time.time() - t0, 2), "size": len(data)})
                marks.append(f"snap {op[1]} at {time.time()-t0:.2f}s size={len(data)}")
                dlog(f"op#{index} snap {op[1]} size={len(data)} -> {path}")
            elif kind == "log":
                # Driver annotation: DRIVER log only. Never pty.log — marks
                # would be replayed into pyte screens as if the app printed them.
                dlog(f"MARK {op[1]}")
            elif kind == "resize":
                kernel32.ResizePseudoConsole(pty.hpc, COORD(op[1], op[2]))
                dlog(f"op#{index} resize {op[1]}x{op[2]}")
            elif kind == "clipset":
                clipset(op[1], shots)
                dlog(f"op#{index} clipset {len(op[1])} chars")
            elif kind == "clipdump":
                text = clipget()
                path = os.path.join(shots, op[1] + ".clip.txt")
                with open(path, "w", encoding="utf-8") as f:
                    f.write(text)
                marks.append(f"clipdump {op[1]}: {text[:200]!r}")
                dlog(f"op#{index} clipdump {op[1]} {text[:80]!r}")
            else:
                raise ValueError(f"unknown op kind: {kind!r}")
        if not timeout_hit and over_budget():
            timeout_hit = True
            marks.append(f"TIMEOUT: {timeout}s buffer exceeded after last op")
            dlog(marks[-1])
    finally:
        # Graceful quit: esc -> short wait -> /quit, ONE retry if the app is
        # still alive, then the loud ctrl+c / terminate escalation. The retry
        # covers a /quit that got appended to leftover editor text (the
        # recorded r2b_s7 lesson); it never loops and every step is marked, so
        # a failure can never be masked (restore_markers fails on any
        # "WARN: still alive" mark).
        def quit_attempt(attempt):
            try:
                pty.write(b"\x1b")
                dlog(f"quit attempt {attempt}: sent esc")
                time.sleep(0.5)  # short wait: let a popup/run abort
                pty.write(b"/quit\r")
                dlog(f"quit attempt {attempt}: sent /quit, waited 3s")
                time.sleep(3)
            except OSError as exc:
                dlog(f"quit attempt {attempt}: write failed: {exc}")

        # LIVENESS = the PROCESS HANDLE, never the pty pipe: a ConPTY keeps its
        # output pipe open after the attached process exits (measured
        # 2026-10-16 with probe_quit_exit.py: a single `/quit` exits the app in
        # 0.5 s while the reader never sees EOF). Pipe-EOF liveness made every
        # scenario report "still alive", run the retry escalation, and mask a
        # genuine never-exits regression behind a force-kill.
        def proc_alive():
            return kernel32.WaitForSingleObject(pty.hproc, 0) != 0

        # Clear the editor before quitting: a scenario that leaves text in it
        # makes the driver's `/quit` append to that text, so Enter submits a
        # PROMPT instead of the command (the recorded r2b_s7 lesson; it was
        # masked by the old pipe-EOF liveness + retry escalation). ctrl+c is
        # upstream `app.clear`; ctrl+u is the readline kill-to-start fallback.
        if proc_alive():
            try:
                pty.write(b"")
                time.sleep(0.25)
                pty.write(b"")
                time.sleep(0.25)
            except OSError:
                pass

        if proc_alive():
            quit_attempt(1)
        if proc_alive():
            marks.append("RETRY: still alive after first esc+/quit; one more esc+/quit")
            dlog(marks[-1])
            quit_attempt(2)
        if proc_alive() and not pty.wait(5):
            marks.append("WARN: still alive after /quit, ctrl+c")
            dlog(marks[-1])
            try:
                pty.write(b"\x03"); time.sleep(2)
            except OSError:
                pass
        if pty.alive and not pty.wait(3):
            marks.append("WARN: still alive, terminating")
            dlog(marks[-1])
            pty.terminate(); pty.wait(3)
        exit_code = pty.exit_code()
        data = pty.snapshot()
        with open(final_path, "wb") as f:
            f.write(data)
        pty.close()
        dlog(f"exit_code={exit_code} final_log={final_path} bytes={len(data)}")
        dlog_fh.close()

    counts = count_restore(data)
    restore = {
        "exit_code": exit_code,
        "alt_screen_exit (ESC[?1049l)": counts["1049l"] > 0,
        "mouse_disable (ESC[?1000l)": counts["1000l"] > 0,
        "mouse_sgr_disable (ESC[?1006l)": counts["1006l"] > 0,
        "cursor_show (ESC[?25h)": counts["25h"] > 0,
        "bracketed_paste_off (ESC[?2004l)": counts["2004l"] > 0,
    }
    result = {
        "scenario": name,
        "pir": PIR,
        "cmdline": cmdline,
        "cwd": target_cwd,
        "agent_dir": env.get("PIR_AGENT_DIR"),
        "timeout_s": timeout,
        "timed_out": timeout_hit,
        "elapsed_s": round(time.time() - t0, 2),
        "exit_code": exit_code,
        "restore": restore,
        "counts": counts,
        "marks": marks,
        "snaps": snaps,
        "final_log": final_path,
        "driver_log": driver_log_path,
    }
    if result_json:
        os.makedirs(os.path.dirname(os.path.abspath(result_json)), exist_ok=True)
        with open(result_json, "w", encoding="utf-8") as f:
            json.dump(result, f, indent=2)
    print(f"=== scenario {name} ===")
    for k, v in restore.items():
        print(f"  {k}: {v}")
    print(f"  timed_out: {timeout_hit}")
    for m in marks:
        print(f"  {m}")
    print(f"  log: {final_path} ({len(data)} bytes)")
    print(f"  driver_log: {driver_log_path}")
    if result_json:
        print(f"  result_json: {result_json}")
    return data

if __name__ == "__main__":
    import argparse
    ap = argparse.ArgumentParser(description="ConPTY driver for pir TUI scenarios")
    ap.add_argument("scenario", help="scenario .py defining NAME + OPS")
    ap.add_argument("--shots-dir", help="dir for <name>.log / <snap>.log (default out/)")
    ap.add_argument("--driver-log", help="driver annotation/trace log (default <shots>/<name>.driver.log)")
    ap.add_argument("--result-json", help="write the machine-readable run result here")
    ap.add_argument("--agent-dir", help="child PIR_AGENT_DIR and default cwd (default out/tui_agent/)")
    ap.add_argument("--timeout", type=float, default=None,
                    help="wall-clock budget in seconds; on expiry the normal quit path runs")
    ap.add_argument("--offline", action="store_true", help="set PI_OFFLINE=1 for the child")
    ap.add_argument("--pir", help="pir.exe override (default: PIR_TUI_EXE or target-alt/debug/pir.exe)")
    a = ap.parse_args()
    if a.pir:
        PIR = a.pir
    ns = {"__file__": os.path.abspath(a.scenario)}
    exec(open(a.scenario, encoding="utf-8").read(), ns)
    timeout = a.timeout if a.timeout is not None else ns.get("TIMEOUT")
    extra_env = dict(ns.get("ENV") or {})
    if a.offline:
        extra_env.setdefault("PI_OFFLINE", "1")
    run_scenario(ns["NAME"], ns["OPS"], extra_env=extra_env, args=ns.get("ARGS"),
                 shots_dir=a.shots_dir, driver_log=a.driver_log, result_json=a.result_json,
                 agent_dir=a.agent_dir, timeout=timeout)
