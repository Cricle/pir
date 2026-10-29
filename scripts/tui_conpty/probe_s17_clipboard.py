# -*- coding: utf-8 -*-
# Probe (2026-10-15, s17 fold-marker calibration, second attempt). Why: on this
# ConPTY the ESC[200~ route cannot reach the editor's paste path (the console
# synthesizes key records and swallows the markers — see
# probe_paste_records.py), but the app's own clipboard paste (alt+v on Windows,
# TuiAction::Paste) calls Editor::paste_text directly, so the fold IS live on
# the current binary. This probe sets the clipboard to a 12-line payload and
# presses alt+v, so the rendered marker can be observed as-is.
NAME = "probe_s17_clipboard"
_PAYLOAD = "\n".join("fold-line-%02d" % i for i in range(1, 13))
OPS = [
    ("wait_quiet", 4),
    ("log", "clipboard 12-line payload -> alt+v -> observe the fold marker"),
    ("clipset", _PAYLOAD),
    ("send", b"\x1bv"),
    ("wait", 1.5),
    ("snap", "probe_clip_after"),
    ("log", "cleanup: ctrl+c clears the editor, backspaces safety net"),
    ("send", b"\x03"),
    ("wait", 0.8),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
    ("snap", "probe_clip_cleared"),
]
CHECKS = []
