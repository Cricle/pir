# -*- coding: utf-8 -*-
# Ported from r2b_s8_session.py. Intent: after one real model round-trip,
# /session shows the Session Info block with the message stats
# ("Assistant: 1") and the Tokens section.
# HERMETIC since 2026-10-15: the battery injects the local mock LLM
# (mock_llm.py) into the seeded agent dir by default, so this scenario runs
# against 127.0.0.1 with no gateway. --real-provider skips the injection for
# the openai-based evidence path. OFFLINE=False so the app really performs the
# (mock) HTTP round-trip.
# CALIBRATED 2026-10-15 (observed against the mock): the assistant text is the
# mock's fixed reply "mock reply" (the old `hello from pir test` expectation
# was the prompt's requested text, which a mock never promises).
NAME = "s08_session_info"
OFFLINE = False
TIMEOUT = 240
OPS = [
    ("wait_quiet", 4),
    ("log", "one model round-trip (mock LLM)"),
    ("send", b"Reply with exactly this one short sentence and nothing else: hello from pir test.\r"),
    ("wait_quiet", 15),
    ("snap", "s08_reply"),
    ("log", "/session"),
    ("send", b"/session\r"),
    ("wait", 2.0),
    ("snap", "s08_session"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]
CHECKS = [
    ("screen_regex", "s08_reply", r"(?i)mock reply"),
    ("screen_contains", "s08_session", "Session Info"),
    ("screen_contains", "s08_session", "Assistant: 1"),
    ("screen_contains", "s08_session", "Tokens"),
    ("restore_markers",),
]
