# -*- coding: utf-8 -*-
# PLAY p11 — a backgrounded tool call (background: true) as a user would see
# it: the `bg: N` marker, the turn's text arriving while the call still runs,
# and the block flipping to the real output afterwards.
NAME = "p11_background"
OFFLINE = False
TIMEOUT = 150
MOCK_SCRIPT = [
    {"tool_call": {"name": "bash",
                   "arguments": {"command": "sleep 6", "background": True}}},
    {"text": "p11-bg-turn-done"},
]
OPS = [
    ("wait_quiet", 4),
    ("send", b"run a background check\r"),
    ("wait", 2.5),
    ("snap", "p11_running"),
    ("wait", 5.0),
    ("wait_quiet", 3),
    ("snap", "p11_settled"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
