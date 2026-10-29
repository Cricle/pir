# -*- coding: utf-8 -*-
# PLAY p15 — quit variant: `/quit` then Esc (close the completion popup) then
# Enter. If the popup swallows Enter, this one should exit on the first Enter.
NAME = "p15_quit_esc_enter"
SEED_AGENT = False
TIMEOUT = 60
OPS = [
    ("wait_quiet", 4),
    ("send", b"/quit"),
    ("wait", 1.0),
    ("snap", "p15_popup_open"),
    ("send", b"\x1b"),
    ("wait", 1.0),
    ("snap", "p15_popup_closed"),
    ("send", b"\r"),
    ("wait", 1.0),
    ("snap", "p15_after_enter"),
    ("wait_quiet", 5),
    ("snap", "p15_after_wait"),
]
CHECKS = [
    ("restore_markers",),
]
