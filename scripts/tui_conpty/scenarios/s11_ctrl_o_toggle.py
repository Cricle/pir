# -*- coding: utf-8 -*-
# New (battery starter). Intent: Ctrl+O toggles the app-level tool-output
# expansion flag. With an empty transcript there is no visible delta, so this
# pins liveness: the "ctrl+o more" header hint survives boot, two toggles and
# the restore toggle, and the app still quits cleanly (restore markers).
NAME = "s11_ctrl_o_toggle"
OPS = [
    ("wait_quiet", 4),
    ("snap", "s11_boot"),
    ("log", "ctrl+o toggle on"),
    ("send", b"\x0f"),
    ("wait", 1.0),
    ("snap", "s11_expanded"),
    ("log", "ctrl+o toggle off"),
    ("send", b"\x0f"),
    ("wait", 1.0),
    ("snap", "s11_restored"),
]
CHECKS = [
    ("screen_contains", "s11_boot", "ctrl+o more"),
    ("screen_contains", "s11_expanded", "ctrl+o more"),
    ("screen_contains", "s11_restored", "ctrl+o more"),
    ("restore_markers",),
]
