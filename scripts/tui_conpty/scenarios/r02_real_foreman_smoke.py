# -*- coding: utf-8 -*-
# REAL-PROVIDER foreman-mode smoke test (uses the configured ZCode LLM).
# Verifies /mode foreman enters, the status badge appears, the model can still
# answer a real prompt in foreman mode, and /mode normal restores baseline.
NAME = "r02_real_foreman_smoke"
NEEDS_REAL_PROVIDER = True
OFFLINE = False
TIMEOUT = 260
OPS = [
    ("wait_quiet", 4),
    ("send", b"/mode foreman\r"),
    ("wait", 1.5),
    ("snap", "r02_foreman_on"),
    ("log", "ask a real question in foreman mode"),
    ("send", b"Reply with exactly: FOREMAN_REAL_OK\r"),
    ("wait", 25.0),
    ("wait_quiet", 6),
    ("snap", "r02_answer"),
    ("send", b"/mode normal\r"),
    ("wait", 1.5),
    ("snap", "r02_normal"),
    ("send", b"/quit\r"),
]
CHECKS = [
    ("screen_contains", "r02_foreman_on", "MODE: FOREMAN"),
    ("screen_contains", "r02_answer", "FOREMAN_REAL_OK"),
    ("screen_contains", "r02_normal", "Normal mode restored"),
    ("screen_not_contains", "r02_normal", "MODE: FOREMAN"),
    ("restore_markers",),
]
