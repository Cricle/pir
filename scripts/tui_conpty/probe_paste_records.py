# -*- coding: utf-8 -*-
# Probe (2026-10-15, s17 newline-loss investigation): dump the raw Windows
# console INPUT_RECORDs that a ConPTY delivers for a bracketed paste, in the
# exact console mode the pir TUI runs in (crossterm raw mode: clear
# ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT; no
# ENABLE_VIRTUAL_TERMINAL_INPUT -- the PIR_TUI_VT_INPUT default).
#
# Usage:
#   python probe_paste_records.py            # parent: spawn the child in a
#                                            # ConPTY, send one paste, print dump
#   python probe_paste_records.py --child    # child role (run by the parent)
#
# The child writes one JSON line per key record to PROBE_DUMP (env), so the
# dump survives even though stdout is the pseudoconsole.
import ctypes, ctypes.wintypes as wt, json, os, sys, time

GENERIC_READ = 0x80000000
GENERIC_WRITE = 0x40000000
FILE_SHARE_READ = 1
FILE_SHARE_WRITE = 2
OPEN_EXISTING = 3
KEY_EVENT = 0x0001
NOT_RAW_MODE_MASK = 0x0001 | 0x0002 | 0x0004  # PROCESSED | LINE | ECHO

class CHAR_UNION(ctypes.Union):
    _fields_ = [("UnicodeChar", wt.WCHAR), ("AsciiChar", ctypes.c_char)]

class KEY_EVENT_RECORD(ctypes.Structure):
    _fields_ = [("bKeyDown", wt.BOOL), ("wRepeatCount", wt.WORD),
                ("wVirtualKeyCode", wt.WORD), ("wVirtualScanCode", wt.WORD),
                ("uChar", CHAR_UNION), ("dwControlKeyState", wt.DWORD)]

class EVENT_UNION(ctypes.Union):
    _fields_ = [("KeyEvent", KEY_EVENT_RECORD), ("pad", ctypes.c_byte * 16)]

class INPUT_RECORD(ctypes.Structure):
    _fields_ = [("EventType", wt.WORD), ("Event", EVENT_UNION)]

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
k32.CreateFileW.restype = wt.HANDLE
k32.CreateFileW.argtypes = [wt.LPCWSTR, wt.DWORD, wt.DWORD, ctypes.c_void_p,
                            wt.DWORD, wt.DWORD, wt.HANDLE]
k32.GetConsoleMode.argtypes = [wt.HANDLE, ctypes.POINTER(wt.DWORD)]
k32.SetConsoleMode.argtypes = [wt.HANDLE, wt.DWORD]
k32.ReadConsoleInputW.argtypes = [wt.HANDLE, ctypes.POINTER(INPUT_RECORD),
                                  wt.DWORD, ctypes.POINTER(wt.DWORD)]

def child():
    dump_path = os.environ["PROBE_DUMP"]
    seconds = float(os.environ.get("PROBE_SECONDS", "14"))
    h = k32.CreateFileW("CONIN$", GENERIC_READ | GENERIC_WRITE,
                        FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, 0, None)
    if h == wt.HANDLE(-1).value:
        raise OSError("CreateFileW(CONIN$) failed err=%d" % ctypes.get_last_error())
    mode = wt.DWORD(0)
    if not k32.GetConsoleMode(h, ctypes.byref(mode)):
        raise OSError("GetConsoleMode failed")
    before = mode.value
    k32.SetConsoleMode(h, before & ~NOT_RAW_MODE_MASK)
    after = wt.DWORD(0)
    k32.GetConsoleMode(h, ctypes.byref(after))
    fh = open(dump_path, "w", encoding="utf-8", buffering=1)
    fh.write(json.dumps({"mode_before": before, "mode_after": after.value}) + "\n")
    deadline = time.time() + seconds
    started = time.time()
    rec = INPUT_RECORD()
    got = wt.DWORD(0)
    while time.time() < deadline:
        if not k32.ReadConsoleInputW(h, ctypes.byref(rec), 1, ctypes.byref(got)) or got.value == 0:
            fh.write(json.dumps({"error": ctypes.get_last_error()}) + "\n")
            break
        if rec.EventType != KEY_EVENT:
            fh.write(json.dumps({"event": rec.EventType}) + "\n")
            continue
        k = rec.Event.KeyEvent
        t_ms = (time.time() - started) * 1000.0
        fh.write(json.dumps({
            "char": k.uChar.UnicodeChar,
            "char_code": ord(k.uChar.UnicodeChar) if k.uChar.UnicodeChar else 0,
            "vk": k.wVirtualKeyCode,
            "scan": k.wVirtualScanCode,
            "down": bool(k.bKeyDown),
            "ctrl_state": k.dwControlKeyState,
            "repeat": k.wRepeatCount,
            "t_ms": round(t_ms, 3),
        }, ensure_ascii=False) + "\n")
    fh.write(json.dumps({"done": True}) + "\n")
    fh.close()

def parent():
    import ast
    import tui_drive
    here = os.path.dirname(os.path.abspath(__file__))
    out_dir = os.path.join(here, "out")
    os.makedirs(out_dir, exist_ok=True)
    paste = os.environ.get("PROBE_PASTE")
    if paste is None:
        paste = repr(b"\x1b[200~" + "\n".join("fold-line-%02d" % i for i in range(1, 4)).encode("ascii") + b"\x1b[201~")
    paste_bytes = ast.literal_eval(paste)
    dump = os.path.join(out_dir, "probe_paste_records.dump.jsonl")
    if os.path.exists(dump):
        os.remove(dump)
    env = dict(tui_drive.BASE_ENV)
    env["PROBE_DUMP"] = dump
    env["PROBE_SECONDS"] = "16"
    cmdline = '"%s" "%s" --child' % (sys.executable, os.path.join(here, "probe_paste_records.py"))
    pty = tui_drive.Pty(cmdline, out_dir, env)
    time.sleep(2.0)
    pty.write(paste_bytes)
    time.sleep(4.0)
    pty.terminate()
    pty.wait(3)
    pty.close()
    records = []
    if os.path.exists(dump):
        for line in open(dump, encoding="utf-8"):
            records.append(json.loads(line))
    with open(os.path.join(out_dir, "probe_paste_records.pty.log"), "wb") as f:
        f.write(pty.snapshot())
    print("paste bytes: %r" % paste_bytes)
    print("mode: %r" % (records[0] if records else None))
    print("%-6s %-4s %-5s %-6s %s" % ("char", "code", "vk", "down", "ctrl_state"))
    for r in records:
        if "char" not in r:
            print("  other: %r" % r)
            continue
        ch = r["char"].replace("\n", "\\n").replace("\r", "\\r").replace("\x00", "\\0").replace("\x1b", "\\e")
        print("%-6s %-4d %-5d %-6s %d%s" % (
            repr(ch)[1:-1], r["char_code"], r["vk"], r["down"], r["ctrl_state"],
            "  <-- CTRL" if r["ctrl_state"] & 0x8 else ""))
    print("--- pty echoed output (first 300B) ---")
    print(pty.snapshot()[:300].decode("utf-8", errors="replace"))

if __name__ == "__main__":
    if "--child" in sys.argv:
        child()
    else:
        parent()
