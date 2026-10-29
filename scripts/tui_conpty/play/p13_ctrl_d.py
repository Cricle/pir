# -*- coding: utf-8 -*-
# PLAY p13 — the other documented exit path: ctrl+d at an idle prompt. Does
# the app quit, and does it restore the terminal (alt screen / paste / cursor)?
NAME = "p13_ctrl_d"
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
