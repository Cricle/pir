# -*- coding: utf-8 -*-
# Race variant of s32: the user's report ("after /resume a prompt produces
# nothing") may depend on timing -- e.g. a prompt submitted immediately after
# the picker's Enter, while the session switch is still settling. This scenario
# resumes and sends the prompt with only a 0.2 s gap, then a second prompt
# after the switch has fully settled, and asserts both replies render.
NAME = "s33_resume_prompt_race"
SEED_SESSION = True
TIMEOUT = 120
OPS = [
    ("wait_quiet", 4),
    ("send", b"/resume\r"),
    ("wait", 2.0),
    ("snap", "s33_picker"),
    ("log", "resume, then IMMEDIATELY prompt (0.2s later)"),
    ("send", b"\r"),
    ("wait", 0.2),
    ("send", b"rapid after resume\r"),
    ("wait", 8.0),
    ("snap", "s33_rapid"),
    ("log", "a second prompt after settling must also run"),
    ("send", b"settled prompt\r"),
    ("wait", 8.0),
    ("snap", "s33_settled"),
]
CHECKS = [
    ("screen_contains", "s33_picker", "Resume a session"),
    ("screen_contains", "s33_rapid", "mock reply"),
    ("screen_contains", "s33_settled", "mock reply"),
    # Alt-screen cycles are not observable through ConPTY (it consumes
    # ESC[?1049h/l); the clean exit is the gate.
    ("restore_markers",),
]
