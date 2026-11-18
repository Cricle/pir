# -*- coding: utf-8 -*-
# REAL-PROVIDER smoke test using the configured ZCode LLM (deepseek-v4.1-flash).
# Not served by the mock; only runs with --real-provider + --models-template.
# Sends a real prompt and asserts a real (non-empty) assistant answer.
NAME = "r01_real_provider_smoke"
NEEDS_REAL_PROVIDER = True
OFFLINE = False
TIMEOUT = 180
ENV = {"PIR_REAL_PROVIDER_SMOKE": "1"}
OPS = [
    ("wait_quiet", 4),
    ("log", "send a real prompt to the configured ZCode LLM"),
    ("send", b"Reply with exactly: REAL_LLM_OK\r"),
    ("wait", 20.0),
    ("wait_quiet", 6),
    ("snap", "r01_answer"),
    ("send", b"/quit\r"),
]
CHECKS = [
    ("screen_contains", "r01_answer", "REAL_LLM_OK"),
    ("restore_markers",),
]
