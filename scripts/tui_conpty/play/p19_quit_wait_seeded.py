# -*- coding: utf-8 -*-
# PLAY p19 — same `/quit` + 20s wait, but with a WORKING provider config
# (seeded + mock injected): does the presence of a live agent session change
# how long /quit takes / whether it exits at all?
NAME = "p19_quit_wait_seeded"
OFFLINE = False
TIMEOUT = 90
MOCK_SCRIPT = [{"text": "p19-reply"}]
OPS = [
    ("wait_quiet", 4),
    ("send", b"p19 prompt\r"),
    ("wait", 3.0),
    ("wait_quiet", 3),
    ("snap", "p19_turn_done"),
    ("send", b"/quit\r"),
    ("wait", 2.0),
    ("snap", "p19_after_enter"),
    ("wait", 20.0),
    ("snap", "p19_after_20s"),
]
CHECKS = [
    ("restore_markers",),
]
