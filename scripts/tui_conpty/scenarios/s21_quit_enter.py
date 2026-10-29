# Promoted from the 2026-10-15 play session (p14_quit_enter.py) into the
# acceptance battery: a real-user regression scenario. Its evidence
# lives in scripts/tui_conpty/play/ and WAVE-REPORT.md §6b.
# -*- coding: utf-8 -*-
# PLAY p14 — quit-path isolation: type `/quit` and press Enter ONCE with the
# `/quit` completion popup open. Does the app exit? (The driver's own quit path
# never shows whether the first Enter is enough, and every baseline run needed
# its one retry, so this isolates the interaction.)
NAME = "s21_quit_enter"
SEED_AGENT = False
TIMEOUT = 60
OPS = [
    ("wait_quiet", 4),
    ("snap", "p14_boot"),
    ("send", b"/quit"),
    ("wait", 1.0),
    ("snap", "p14_popup_open"),
    ("send", b"\r"),
    ("wait", 1.0),
    ("snap", "p14_after_enter1"),
    ("wait_quiet", 5),
    ("snap", "p14_after_wait"),
]
CHECKS = [
    ("restore_markers",),
]
