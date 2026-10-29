# -*- coding: utf-8 -*-
# PLAY p07 — a real tool call round-trip via the mock: bash echoes, then the
# final text. Ctrl+O to expand/collapse the tool block afterwards.
NAME = "p07_turn_tool"
OFFLINE = False
TIMEOUT = 120
MOCK_SCRIPT = [
    {"tool_call": {"name": "bash", "arguments": {"command": "echo p07-tool-ok"}}},
    {"text": "p07-turn-done"},
]
OPS = [
    ("wait_quiet", 4),
    ("send", b"run the p07 tool\r"),
    ("wait", 3.0),
    ("snap", "p07_after_tool"),
    ("wait", 3.0),
    ("wait_quiet", 3),
    ("snap", "p07_done"),
    ("send", b"\x0f"),
    ("wait", 1.2),
    ("snap", "p07_expanded"),
    ("send", b"\x0f"),
    ("wait", 1.2),
    ("snap", "p07_collapsed"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
