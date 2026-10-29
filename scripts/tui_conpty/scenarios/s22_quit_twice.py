# Promoted from the 2026-10-15 play session (p20_quit_twice.py) into the
# acceptance battery: a real-user regression scenario. Its evidence
# lives in scripts/tui_conpty/play/ and WAVE-REPORT.md §6b.
# -*- coding: utf-8 -*-
# PLAY p20 — is the second `/quit` (no Esc in between) what actually exits?
# A user whose `/quit` did nothing will simply type `/quit` again.
NAME = "s22_quit_twice"
SEED_AGENT = False
TIMEOUT = 90
OPS = [
    ("wait_quiet", 4),
    ("send", b"/quit\r"),
    ("wait", 6.0),
    ("snap", "p20_first_quit"),
    ("send", b"\x7f" * 12),
    ("wait", 0.5),
    ("send", b"/quit\r"),
    ("wait", 6.0),
    ("snap", "p20_second_quit"),
]
CHECKS = [
    ("restore_markers",),
]
