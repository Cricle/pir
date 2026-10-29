# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/r2b_s8b_session_cost.py (copied 2026-10-15) into
# scripts/tui_conpty/. Scenario data unchanged (provenance header only).
# P3c: with model rates configured, one model round-trip then /session must
# include the "Cost" section with "Total: $...".
NAME = "r2b_s8b_session_cost"
OPS = [
    ("wait_quiet", 4),
    ("log", "send prompt"),
    ("send", b"Reply with exactly this one short sentence and nothing else: hello from pir test.\r"),
    ("wait_quiet", 10),
    ("snap", "r2b_s8b_reply"),
    ("log", "/session"),
    ("send", b"/session\r"),
    ("wait", 2.0),
    ("snap", "r2b_s8b_session_after"),
]
