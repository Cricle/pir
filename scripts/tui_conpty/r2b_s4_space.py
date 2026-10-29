# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/r2b_s4_space.py (copied 2026-10-15) into
# scripts/tui_conpty/. Scenario data unchanged (provenance header only).
# P2-2: typing "XY " (word + space) must NOT pop the whole-cwd file listing.
NAME = "r2b_s4_space"
OPS = [
    ("wait_quiet", 4),
    ("log", "before XY space"),
    ("send", b"XY "),
    ("wait", 1.5),
    ("snap", "r2b_s4_space_after"),
    ("log", "after XY space"),
]
