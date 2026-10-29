# -*- coding: utf-8 -*-
# Ported from r2b_s8b_session_cost.py. Intent: with model rates configured in
# the seeded models.json, one model round-trip then /session must show the
# Cost section with a "Total: $..." row.
# HERMETIC since 2026-10-15: the battery injects the local mock LLM
# (mock_llm.py) into the seeded agent dir by default and adds non-zero cost
# rates to any model lacking them (the /session Cost block renders only when
# cost > 0), so this runs against 127.0.0.1. --real-provider skips the
# injection for the openai-based evidence path. OFFLINE=False so the app really
# performs the (mock) HTTP round-trip.
NAME = "s08b_session_cost"
OFFLINE = False
TIMEOUT = 240
OPS = [
    ("wait_quiet", 4),
    ("log", "one model round-trip (mock LLM)"),
    ("send", b"Reply with exactly this one short sentence and nothing else: hello from pir test.\r"),
    ("wait_quiet", 15),
    ("snap", "s08b_reply"),
    ("log", "/session"),
    ("send", b"/session\r"),
    ("wait", 2.0),
    ("snap", "s08b_session"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]
CHECKS = [
    ("screen_contains", "s08b_session", "Session Info"),
    ("screen_contains", "s08b_session", "Cost"),
    ("screen_regex", "s08b_session", r"Total: \$"),
    ("restore_markers",),
]
