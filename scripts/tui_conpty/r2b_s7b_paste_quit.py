# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/r2b_s7b_paste_quit.py (copied 2026-10-15) into
# scripts/tui_conpty/. Scenario data unchanged (provenance header only).
# P3b (dedup): bracketed paste renders once; clean /quit after clearing the
# editor so the final log carries the exit sequences for the ESC[?2004l count.
NAME = "r2b_s7b_paste_quit"
OPS = [
    ("wait_quiet", 4),
    ("log", "bracketed paste PASTE4242"),
    ("send", b"\x1b[200~PASTE4242\x1b[201~"),
    ("wait", 1.5),
    ("snap", "r2b_s7b_paste_after"),
    ("log", "clear editor then quit"),
    ("send", b"\x7f" * 9),
    ("wait", 0.8),
    ("snap", "r2b_s7b_cleared"),
]
