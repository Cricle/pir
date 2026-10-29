# -*- coding: utf-8 -*-
# `/clear` parity with upstream `handleClearCommand`: it starts a NEW session
# (not just clearing the visible transcript), so the previous turn is gone and
# the session stats restart from zero. The native lane's unit tests pin the
# state reset; this scenario proves it in a real TUI over ConPTY.
# HERMETIC: the battery injects the local mock LLM by default.
NAME = "s36_clear_new_session"
OFFLINE = False
TIMEOUT = 240
OPS = [
    ("wait_quiet", 4),
    ("log", "one mock round-trip so there is usage to reset"),
    ("send", b"Reply with exactly this one short sentence and nothing else: hello from pir test.\r"),
    ("wait_quiet", 15),
    ("snap", "s36_reply"),
    ("log", "/clear"),
    ("send", b"/clear\r"),
    ("wait", 3.0),
    ("snap", "s36_clear"),
    ("log", "/session after the new session"),
    ("send", b"/session\r"),
    ("wait", 2.0),
    ("snap", "s36_info"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 30),
    ("wait", 0.6),
]
CHECKS = [
    ("screen_regex", "s36_reply", r"(?i)mock reply"),
    ("screen_contains", "s36_clear", "New session started"),
    ("screen_not_contains", "s36_clear", "mock reply"),
    ("screen_contains", "s36_info", "Session Info"),
    ("screen_contains", "s36_info", "Assistant: 0"),
    ("restore_markers",),
]
