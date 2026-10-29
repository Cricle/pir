# -*- coding: utf-8 -*-
# PLAY p01 — nonsense / empty slash commands a real user types by mistake.
# Provider-free (SEED_AGENT=False, no turn is submitted): pure UI reaction.
NAME = "p01_slash_bad"
SEED_AGENT = False
TIMEOUT = 90
OPS = [
    ("wait_quiet", 4),
    ("send", b"/nope\r"),
    ("wait", 2.0),
    ("snap", "p01_nope"),
    ("send", b"/skill:nope\r"),
    ("wait", 2.0),
    ("snap", "p01_skill_nope"),
    ("send", b"/\r"),
    ("wait", 2.0),
    ("snap", "p01_bare_slash"),
    ("send", b"/ \r"),
    ("wait", 2.0),
    ("snap", "p01_slash_space"),
    ("send", b"\x03"),          # ctrl+c: clear whatever is left
    ("wait", 0.6),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
]
CHECKS = [
    ("restore_markers",),
]
