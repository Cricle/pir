# -*- coding: utf-8 -*-
# PLAY p18 — is `/quit` "slow" or "ignored"? Type `/quit` + Enter ONCE, then
# wait 20s with no other input. If the app exits on its own the driver finds
# the pty dead and its quit path (and the RETRY mark) never runs.
NAME = "p18_quit_wait"
SEED_AGENT = False
TIMEOUT = 90
OPS = [
    ("wait_quiet", 4),
    ("send", b"/quit\r"),
    ("wait", 2.0),
    ("snap", "p18_after_enter"),
    ("wait", 20.0),
    ("snap", "p18_after_20s"),
]
CHECKS = [
    ("restore_markers",),
]
