# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/r2b_s6_settings.py (copied 2026-10-15) into
# scripts/tui_conpty/. Scenario data unchanged (provenance header only).
# P3a: /settings dialog; changing a value must show a notice with the exact
# settings.json key path ("Saved setting: <path>").
NAME = "r2b_s6_settings"
OPS = [
    ("wait_quiet", 4),
    ("log", "open /settings"),
    ("send", b"/settings\r"),
    ("wait", 1.5),
    ("snap", "r2b_s6_settings_open"),
    ("log", "down down (to a value row)"),
    ("send", b"\x1b[B\x1b[B"),
    ("wait", 0.6),
    ("snap", "r2b_s6_settings_nav"),
    ("log", "right = change value"),
    ("send", b"\x1b[C"),
    ("wait", 1.2),
    ("snap", "r2b_s6_settings_changed"),
    ("log", "esc close"),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("snap", "r2b_s6_settings_closed"),
]
