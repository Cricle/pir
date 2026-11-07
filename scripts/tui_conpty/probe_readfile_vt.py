# -*- coding: utf-8 -*-
"""Probe: does ReadFile(CONIN$) deliver the original VT byte stream when
ENABLE_VIRTUAL_TERMINAL_INPUT is set?

Parent spawns the child under a ConPTY, sends one bracketed paste with a
control byte, and the child dumps every block ReadFile returns.
"""
import ctypes, ctypes.wintypes as wt, json, os, sys, time

GENERIC_READ = 0x80000000
GENERIC_WRITE = 0x40000000
FILE_SHARE_READ = 1
FILE_SHARE_WRITE = 2
OPEN_EXISTING = 3
ENABLE_VIRTUAL_TERMINAL_INPUT = 0x0200
NOT_RAW_MODE_MASK = 0x0001 | 0x0002 | 0x0004
ENABLE_PROCESSED_INPUT = 0x0001
ENABLE_LINE_INPUT = 0x0002
ENABLE_ECHO_INPUT = 0x0004

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
k32.CreateFileW.restype = wt.HANDLE
k32.CreateFileW.argtypes = [wt.LPCWSTR, wt.DWORD, wt.DWORD, ctypes.c_void_p,
                            wt.DWORD, wt.DWORD, wt.HANDLE]
k32.GetConsoleMode.argtypes = [wt.HANDLE, ctypes.POINTER(wt.DWORD)]
k32.SetConsoleMode.argtypes = [wt.HANDLE, wt.DWORD]
k32.ReadFile.restype = wt.BOOL
k32.ReadFile.argtypes = [wt.HANDLE, ctypes.c_void_p, wt.DWORD,
                         ctypes.POINTER(wt.DWORD), ctypes.c_void_p]


def child():
    dump_path = os.environ["PROBE_DUMP"]
    seconds = float(os.environ.get("PROBE_SECONDS", "8"))
    h = k32.CreateFileW("CONIN$", GENERIC_READ | GENERIC_WRITE,
                        FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, 0, None)
    if h == wt.HANDLE(-1).value:
        raise OSError("CreateFileW(CONIN$) failed err=%d" % ctypes.get_last_error())
    mode = wt.DWORD(0)
    if not k32.GetConsoleMode(h, ctypes.byref(mode)):
        raise OSError("GetConsoleMode failed")
    before = mode.value
    want = (before & ~NOT_RAW_MODE_MASK) | ENABLE_VIRTUAL_TERMINAL_INPUT
    if not k32.SetConsoleMode(h, want):
        raise OSError("SetConsoleMode failed err=%d" % ctypes.get_last_error())
    after = wt.DWORD(0)
    k32.GetConsoleMode(h, ctypes.byref(after))
    fh = open(dump_path, "w", encoding="utf-8", buffering=1)
    fh.write(json.dumps({"mode_before": before, "mode_after": after.value}) + "\n")
    buf = ctypes.create_string_buffer(4096)
    read = wt.DWORD(0)
    deadline = time.time() + seconds
    idx = 0
    while time.time() < deadline:
        ok = k32.ReadFile(h, buf, 4096, ctypes.byref(read), None)
        if not ok:
            fh.write(json.dumps({"error": ctypes.get_last_error()}) + "\n")
            break
        data = buf.raw[:read.value]
        idx += 1
        fh.write(json.dumps({"i": idx, "len": len(data), "hex": data.hex(),
                             "repr": repr(data)}) + "\n")
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
        paste = repr(b"\x1b[200~AAA\x03BBB\x1b[201~")
    paste_bytes = ast.literal_eval(paste)
    dump = os.path.join(out_dir, "probe_readfile_vt.dump.jsonl")
    if os.path.exists(dump):
        os.remove(dump)
    env = dict(tui_drive.BASE_ENV)
    env["PROBE_DUMP"] = dump
    env["PROBE_SECONDS"] = "6"
    cmdline = '"%s" "%s" --child' % (sys.executable, os.path.join(here, "probe_readfile_vt.py"))
    pty = tui_drive.Pty(cmdline, out_dir, env)
    time.sleep(1.5)
    pty.write(paste_bytes)
    time.sleep(2.0)
    pty.terminate()
    pty.wait(3)
    pty.close()
    print("paste bytes: %r" % paste_bytes)
    if os.path.exists(dump):
        for line in open(dump, encoding="utf-8"):
            print(line.rstrip())


if __name__ == "__main__":
    if "--child" in sys.argv:
        child()
    else:
        parent()
