# -*- coding: utf-8 -*-
# Foreman verification for the play lane's F2: with no model configured, a
# prompt produces a worker-level error — the working indicator must STOP, not
# spin forever.
NAME = "p23_worker_error_spinner"
SEED_AGENT = False
TIMEOUT = 45
OPS = [
    ("wait_quiet", 4),
    ("send", b"hello\r"),
    ("wait", 6.0),
    ("snap", "p23_after_error"),
]
CHECKS = [
    ("screen_not_contains", "p23_after_error", "Working"),
]
