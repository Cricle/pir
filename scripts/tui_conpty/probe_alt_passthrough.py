# -*- coding: utf-8 -*-
"""Probe: does this ConPTY layer forward ESC[?1049h / ESC[?1049l from a child
at all? Runs a tiny Python child that writes both sequences plus a marker,
then exits. Used to decide whether the battery's alt-screen expectation can
ever be observed through tui_drive.Pty."""
import os
import sys
import time

import tui_drive


def main():
    child = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                         "probe_alt_child.py")
    cmdline = f'"{sys.executable}" "{child}"'
    pty = tui_drive.Pty(cmdline, os.getcwd(), tui_drive.BASE_ENV)
    try:
        pty.wait(10)
        time.sleep(0.5)
        data = pty.snapshot()
    finally:
        pty.close()
    print("bytes:", len(data))
    for seq in (b"\x1b[?1049h", b"\x1b[?1049l", b"ALT-PROBE-OK"):
        print(repr(seq), data.count(seq))
    print("tail:", repr(data[-200:]))


if __name__ == "__main__":
    main()
