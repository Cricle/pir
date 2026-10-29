# -*- coding: utf-8 -*-
# PLAY p04 — does a plain typed `/quit` + Enter actually quit, and how long
# does it take? No esc preamble (the driver's own quit path always sends esc
# first, which masks this). If the app exits during the ops, the driver's
# finally-block finds it dead and no RETRY mark appears.
NAME = "p04_quit_direct"
SEED_AGENT = False
TIMEOUT = 60
OPS = [
    ("wait_quiet", 4),
    ("snap", "p04_boot"),
    ("log", "type /quit + Enter, then let the app exit on its own"),
    ("send", b"/quit\r"),
    ("wait", 1.0),
    ("snap", "p04_after_enter"),
    ("wait_quiet", 5),
    ("snap", "p04_after_wait"),
]
CHECKS = [
    ("restore_markers",),
]
