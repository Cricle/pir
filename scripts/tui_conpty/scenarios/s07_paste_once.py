# -*- coding: utf-8 -*-
# Ported from r2b_s7_paste.py / r2b_s7b_paste_quit.py. Intent: a bracketed
# paste (ESC[200~...ESC[201~) renders the text exactly ONCE in the editor
# (no duplication), and the editor is then cleared so the driver's /quit
# exits cleanly and the final log carries ESC[?2004l exactly once
# (restore_markers). r2b_s7's original leftover-text variant could not quit
# cleanly - the clear step is the recorded r2b_s7b lesson.
# EXTENDED 2026-10-15 (paste-gate fix hand-off): the paste markers must never
# leak into the rendered UI, and the editor must hold EXACTLY the 10-char
# payload (row regex; observed row ` PASTE4242` between the box rules).
NAME = "s07_paste_once"
OPS = [
    ("wait_quiet", 4),
    ("log", "bracketed paste PASTE4242"),
    ("send", b"\x1b[200~PASTE4242\x1b[201~"),
    ("wait", 1.5),
    ("snap", "s07_after"),
    ("log", "clear the editor so /quit exits cleanly"),
    ("send", b"\x7f" * 19),
    ("wait", 0.8),
    ("snap", "s07_cleared"),
]
CHECKS = [
    ("screen_count", "s07_after", "PASTE4242", 1),
    ("editor_contains", "s07_after", "PASTE4242"),
    ("screen_regex", "s07_after", r"(?m)^[ \t]*PASTE4242[ \t]*$"),
    ("screen_not_contains", "s07_after", "[200~"),
    ("screen_not_contains", "s07_after", "[201~"),
    ("screen_not_contains", "s07_cleared", "PASTE4242"),
    ("restore_markers",),
]
