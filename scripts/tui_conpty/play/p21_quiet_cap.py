# -*- coding: utf-8 -*-
# Foreman check for the wait_quiet cap (play-lane F6): a long paced stream keeps
# the screen changing, so `wait_quiet` must end AT-CAP (bounded), never hang.
NAME = "p21_quiet_cap"
MOCK_SCRIPT = [{"text": " ".join(f"cap{i}" for i in range(400)), "chunks": 200, "chunk_delay_ms": 50}]
OPS = [
    ("wait_quiet", 3),
    ("send", b"go\r"),
    ("wait_quiet", 0.3),
    ("snap", "p21_during"),
]
CHECKS = [("restore_markers",)]
TIMEOUT = 16
