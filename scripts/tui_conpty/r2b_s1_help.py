# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/r2b_s1_help.py (copied 2026-10-15) into
# scripts/tui_conpty/. Scenario data unchanged (provenance header only).
# P1-1: /help notice must render in the snapshot without any further key events.
NAME = "r2b_s1_help"
OPS = [
    ("wait_quiet", 4),
    ("log", "before /help"),
    ("send", b"/help\r"),
    ("wait", 1.5),
    ("snap", "r2b_s1_help_after"),
    ("log", "after /help"),
]
