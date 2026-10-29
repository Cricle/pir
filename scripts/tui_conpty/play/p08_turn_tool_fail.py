# -*- coding: utf-8 -*-
# PLAY p08 — a FAILING tool call (nonzero exit) followed by the model's text:
# how does the failure render, does the turn end cleanly?
NAME = "p08_turn_tool_fail"
OFFLINE = False
TIMEOUT = 120
MOCK_SCRIPT = [
    {"tool_call": {"name": "bash", "arguments": {"command": "echo p08-stderr 1>&2; exit 3"}}},
    {"text": "p08-after-failure"},
]
OPS = [
    ("wait_quiet", 4),
    ("send", b"run the failing tool\r"),
    ("wait", 4.0),
    ("wait_quiet", 3),
    ("snap", "p08_fail"),
    ("send", b"\x0f"),
    ("wait", 1.2),
    ("snap", "p08_expanded"),
    ("send", b"\x0f"),
    ("wait", 1.0),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
