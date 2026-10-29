# -*- coding: utf-8 -*-
# PLAY p17 — provider-backed selectors: /model lists the mock's models, arrow
# navigation, Enter selects one; then /thinking Enter selects a level.
NAME = "p17_selectors"
OFFLINE = False
TIMEOUT = 150
OPS = [
    ("wait_quiet", 4),
    ("snap", "p17_boot"),
    ("send", b"/model\r"),
    ("wait", 2.5),
    ("snap", "p17_model_open"),
    ("send", b"\x1b[B"),
    ("wait", 0.8),
    ("snap", "p17_model_down"),
    ("send", b"\r"),
    ("wait", 2.5),
    ("snap", "p17_model_selected"),
    ("send", b"/thinking\r"),
    ("wait", 2.5),
    ("snap", "p17_thinking_open"),
    ("send", b"\x1b[A"),
    ("wait", 0.8),
    ("snap", "p17_thinking_up"),
    ("send", b"\x1b"),
    ("wait", 1.5),
    ("snap", "p17_thinking_cancelled"),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
