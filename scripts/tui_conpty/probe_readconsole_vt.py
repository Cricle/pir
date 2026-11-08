# -*- coding: utf-8 -*-
"""Probe: with ENABLE_VIRTUAL_TERMINAL_INPUT, what do ReadConsoleInputW
INPUT_RECORDs carry for a bracketed paste / control bytes / mouse / resize?"""
import ctypes, ctypes.wintypes as wt, json, os, sys, time
GENERIC_READ=0x80000000; GENERIC_WRITE=0x40000000
FILE_SHARE_READ=1; FILE_SHARE_WRITE=2; OPEN_EXISTING=3
ENABLE_VIRTUAL_TERMINAL_INPUT=0x0200
NOT_RAW_MODE_MASK=0x0001|0x0002|0x0004

class CHAR_UNION(ctypes.Union):
    _fields_=[("UnicodeChar",wt.WCHAR),("AsciiChar",ctypes.c_char)]
class KEY_EVENT_RECORD(ctypes.Structure):
    _fields_=[("bKeyDown",wt.BOOL),("wRepeatCount",wt.WORD),("wVirtualKeyCode",wt.WORD),
              ("wVirtualScanCode",wt.WORD),("uChar",CHAR_UNION),("dwControlKeyState",wt.DWORD)]
class EVENT_UNION(ctypes.Union):
    _fields_=[("KeyEvent",KEY_EVENT_RECORD),("pad",ctypes.c_byte*16)]
class INPUT_RECORD(ctypes.Structure):
    _fields_=[("EventType",wt.WORD),("Event",EVENT_UNION)]
k32=ctypes.WinDLL("kernel32",use_last_error=True)
k32.CreateFileW.restype=wt.HANDLE
k32.CreateFileW.argtypes=[wt.LPCWSTR,wt.DWORD,wt.DWORD,ctypes.c_void_p,wt.DWORD,wt.DWORD,wt.HANDLE]
k32.GetConsoleMode.argtypes=[wt.HANDLE,ctypes.POINTER(wt.DWORD)]
k32.SetConsoleMode.argtypes=[wt.HANDLE,wt.DWORD]
k32.ReadConsoleInputW.argtypes=[wt.HANDLE,ctypes.POINTER(INPUT_RECORD),wt.DWORD,ctypes.POINTER(wt.DWORD)]

def child():
    dump=os.environ["PROBE_DUMP"]; seconds=float(os.environ.get("PROBE_SECONDS","10"))
    h=k32.CreateFileW("CONIN$",GENERIC_READ|GENERIC_WRITE,FILE_SHARE_READ|FILE_SHARE_WRITE,None,OPEN_EXISTING,0,None)
    if h==wt.HANDLE(-1).value: raise OSError("open CONIN$")
    mode=wt.DWORD(0); k32.GetConsoleMode(h,ctypes.byref(mode)); before=mode.value
    want=(before & ~NOT_RAW_MODE_MASK)|ENABLE_VIRTUAL_TERMINAL_INPUT
    if not k32.SetConsoleMode(h,want): raise OSError("set mode %d"%ctypes.get_last_error())
    after=wt.DWORD(0); k32.GetConsoleMode(h,ctypes.byref(after))
    fh=open(dump,"w",encoding="utf-8",buffering=1)
    fh.write(json.dumps({"mode_before":before,"mode_after":after.value})+"\n")
    deadline=time.time()+seconds; rec=INPUT_RECORD(); got=wt.DWORD(0); idx=0
    while time.time()<deadline:
        if not k32.ReadConsoleInputW(h,ctypes.byref(rec),1,ctypes.byref(got)) or got.value==0:
            fh.write(json.dumps({"error":ctypes.get_last_error()})+"\n"); break
        idx+=1
        if rec.EventType!=1:
            fh.write(json.dumps({"i":idx,"event":rec.EventType,"raw":bytes(rec)[:20].hex()})+"\n"); continue
        k=rec.Event.KeyEvent
        fh.write(json.dumps({"i":idx,"char":k.uChar.UnicodeChar,"char_code":ord(k.uChar.UnicodeChar) if k.uChar.UnicodeChar else 0,
                             "vk":k.wVirtualKeyCode,"down":bool(k.bKeyDown),"ctrl":k.dwControlKeyState,"repeat":k.wRepeatCount})+"\n")
    fh.write(json.dumps({"done":True})+"\n"); fh.close()

def parent():
    import tui_drive
    here=os.path.abspath('.')
    out=os.path.join(here,'out'); os.makedirs(out,exist_ok=True)
    dump=os.path.join(out,'probe_readconsole_vt.dump.jsonl')
    if os.path.exists(dump): os.remove(dump)
    env=dict(tui_drive.BASE_ENV); env['PROBE_DUMP']=dump; env['PROBE_SECONDS']='10'
    cmd='"{}" "{}" --child'.format(sys.executable, os.path.join(here,'probe_readconsole_vt.py'))
    p=tui_drive.Pty(cmd,out,env); time.sleep(1.5)
    p.write(b'\x1b[200~AAA\x03BBB\x1b[201~'); time.sleep(0.7)
    p.write(b'\x1b[<0;10;5M\x1b[A'); time.sleep(0.7)
    tui_drive.kernel32.ResizePseudoConsole(p.hpc, tui_drive.COORD(100,30)); time.sleep(0.7)
    p.write(b'Z'); time.sleep(1.0)
    p.terminate(); p.wait(3); p.close()
    for line in open(dump,encoding='utf-8'): print(line.rstrip())

if __name__=="__main__":
    child() if "--child" in sys.argv else parent()
