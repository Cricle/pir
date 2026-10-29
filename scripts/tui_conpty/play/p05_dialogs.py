# -*- coding: utf-8 -*-
# PLAY p05 — dialog/selector navigation a user does: open, arrow, cancel,
# reopen. /model, /thinking, /settings, /tree. Provider-free (no turn).
NAME = "p05_dialogs"
SEED_AGENT = False
TIMEOUT = 150
OPS = [
    ("wait_quiet", 4),
    ("send", b"/model\r"),
    ("wait", 2.0),
    ("snap", "p05_model_open"),
    ("send", b"\x1b[B"),
    ("wait", 0.6),
    ("snap", "p05_model_down"),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("snap", "p05_model_cancelled"),
    ("send", b"/thinking\r"),
    ("wait", 2.0),
    ("snap", "p05_thinking_open"),
    ("send", b"\x1b[B"),
    ("wait", 0.6),
    ("snap", "p05_thinking_down"),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("snap", "p05_thinking_cancelled"),
    ("send", b"/settings\r"),
    ("wait", 2.0),
    ("snap", "p05_settings_open"),
    ("send", b"\x1b[B"),
    ("wait", 0.6),
    ("snap", "p05_settings_down"),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("snap", "p05_settings_cancelled"),
    ("send", b"/tree\r"),
    ("wait", 2.0),
    ("snap", "p05_tree_open"),
    ("send", b"\x1b"),
    ("wait", 1.2),
    ("snap", "p05_tree_cancelled"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
