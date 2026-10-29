# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/r2b_s7_paste.py (copied 2026-10-15) into
# scripts/tui_conpty/. Scenario data unchanged (provenance header only).
# P3b: bracketed paste (ESC[200~...ESC[201~) renders once; on /quit the
# disable sequence ESC[?2004l must be emitted exactly once (dedup).
NAME = "r2b_s7_paste"
OPS = [
    ("wait_quiet", 4),
    ("log", "bracketed paste PASTE4242"),
    ("send", b"\x1b[200~PASTE4242\x1b[201~"),
    ("wait", 1.5),
    ("snap", "r2b_s7_paste_after"),
    ("log", "quit for restore-sequence count"),
]
