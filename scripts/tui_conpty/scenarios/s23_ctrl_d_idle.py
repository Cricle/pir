# Promoted from the 2026-10-15 play session (p13_ctrl_d.py) into the
# acceptance battery: a real-user regression scenario. Its evidence
# lives in scripts/tui_conpty/play/ and WAVE-REPORT.md §6b.
# -*- coding: utf-8 -*-
# PLAY p13 — the other documented exit path: ctrl+d at an idle prompt. Does
# the app quit, and does it restore the terminal (alt screen / paste / cursor)?
NAME = "s23_ctrl_d_idle"
SEED_AGENT = False
TIMEOUT = 60
OPS = [
    ("wait_quiet", 4),
    ("snap", "p13_boot"),
    ("log", "ctrl+d at an idle prompt"),
    ("send", b"\x04"),
    ("wait", 1.0),
    ("snap", "p13_after_ctrl_d"),
    ("wait_quiet", 5),
    ("snap", "p13_after_wait"),
]
CHECKS = [
    ("restore_markers",),
]
