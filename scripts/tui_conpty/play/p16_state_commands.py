# -*- coding: utf-8 -*-
# PLAY p16 — state commands against a WORKING provider: one mock turn first so
# there is a transcript, then /session, /tree, /clear, /name, /new.
NAME = "p16_state_commands"
OFFLINE = False
TIMEOUT = 150
MOCK_SCRIPT = [
    {"text": "p16-reply-one"},
]
OPS = [
    ("wait_quiet", 4),
    ("send", b"first p16 prompt\r"),
    ("wait", 3.0),
    ("wait_quiet", 3),
    ("snap", "p16_turn_done"),
    ("send", b"/session\r"),
    ("wait", 2.0),
    ("snap", "p16_session"),
    ("send", b"/tree\r"),
    ("wait", 2.5),
    ("snap", "p16_tree"),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("send", b"/clear\r"),
    ("wait", 2.5),
    ("snap", "p16_cleared"),
    ("send", b"/name p16-renamed\r"),
    ("wait", 2.0),
    ("snap", "p16_named"),
    ("send", b"/new\r"),
    ("wait", 2.5),
    ("snap", "p16_new"),
    ("send", b"/help\r"),
    ("wait", 1.5),
    ("snap", "p16_help"),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
