# -*- coding: utf-8 -*-
# PLAY p10 — layout: resize the terminal MID-TURN while a paced stream renders,
# then resize back after the turn; then a prompt whose reply is very long
# (scroll behaviour).
NAME = "p10_resize"
OFFLINE = False
TIMEOUT = 150
MOCK_SCRIPT = [
    {"text": "p10-resize-stream " * 20, "chunks": 20, "chunk_delay_ms": 150},
]
OPS = [
    ("wait_quiet", 4),
    ("send", b"start p10 stream\r"),
    ("wait", 1.0),
    ("snap", "p10_before_resize"),
    ("resize", 90, 24),
    ("wait", 2.0),
    ("snap", "p10_resized_midstream"),
    ("wait", 4.0),
    ("wait_quiet", 3),
    ("snap", "p10_resized_done"),
    ("resize", 120, 40),
    ("wait", 2.0),
    ("snap", "p10_restored_size"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
