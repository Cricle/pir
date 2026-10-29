# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/r2b_s5_compact.py (copied 2026-10-15) into
# scripts/tui_conpty/. Scenario data unchanged (provenance header only).
# P2-1: /compact on a fresh (small) session must surface the error
# ("compact failed: nothing to compact (session too small)"), not false success.
NAME = "r2b_s5_compact"
OPS = [
    ("wait_quiet", 4),
    ("log", "before /compact"),
    ("send", b"/compact\r"),
    ("wait", 2.5),
    ("snap", "r2b_s5_compact_after"),
    ("log", "after /compact"),
]
