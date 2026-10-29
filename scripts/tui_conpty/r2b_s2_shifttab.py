# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/r2b_s2_shifttab.py (copied 2026-10-15) into
# scripts/tui_conpty/. Scenario data unchanged (provenance header only).
# P1-2: ESC[Z (BackTab) must cycle thinking; "Thinking level: ..." notice in same snapshot.
NAME = "r2b_s2_shifttab"
OPS = [
    ("wait_quiet", 4),
    ("log", "before shift+tab"),
    ("send", b"\x1b[Z"),
    ("wait", 1.5),
    ("snap", "r2b_s2_shifttab_after"),
    ("log", "after shift+tab"),
]
