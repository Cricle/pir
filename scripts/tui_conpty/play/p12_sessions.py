# -*- coding: utf-8 -*-
# PLAY p12 — sessions a user juggles: /resume picker (list, navigate, cancel,
# reopen, Enter to actually resume), then /fork on the resumed session.
NAME = "p12_sessions"
SEED_SESSION = True
TIMEOUT = 150
OPS = [
    ("wait_quiet", 4),
    ("send", b"/resume\r"),
    ("wait", 2.5),
    ("snap", "p12_picker"),
    ("send", b"\x1b[B"),
    ("wait", 0.8),
    ("snap", "p12_picker_down"),
    ("send", b"\x1b[B"),
    ("wait", 0.8),
    ("snap", "p12_picker_down2"),
    ("send", b"\x1b[A"),
    ("wait", 0.8),
    ("send", b"\x1b"),
    ("wait", 1.5),
    ("snap", "p12_cancelled"),
    ("send", b"/resume\r"),
    ("wait", 2.5),
    ("send", b"\r"),
    ("wait", 3.0),
    ("snap", "p12_resumed"),
    ("send", b"/fork\r"),
    ("wait", 2.5),
    ("snap", "p12_forked"),
    ("send", b"/name p12-name\r"),
    ("wait", 2.0),
    ("snap", "p12_named"),
    ("send", b"/session\r"),
    ("wait", 2.0),
    ("snap", "p12_session_info"),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers", {"alt_screen_exit": 2}),
]
