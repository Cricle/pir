# -*- coding: utf-8 -*-
# Ported from r2b_s1_help.py (recorded 2026-10-10, re-run 2026-10-15).
# Intent: /help must EXECUTE on Enter and render the command notice
# ("Commands: /model /thinking /clear /compact ..."); the completion popup
# (row "-> help  Show available commands") must be gone afterwards.
# Freeze note: on the current P0 build Enter never submits, so /help stays in
# the editor with the popup open and this scenario FAILs on the content check
# (restore markers fail too, because /quit never executes).
NAME = "s01_help_overlay"
OPS = [
    ("wait_quiet", 4),
    ("log", "boot settled"),
    ("send", b"/help\r"),
    ("wait", 1.5),
    ("snap", "s01_after"),
    ("log", "cleanup editor (no-op on a healthy build: command cleared it)"),
    ("send", b"\x7f" * 24),
    ("wait", 0.6),
]
CHECKS = [
    ("screen_contains", "s01_after", "Commands: /model /thinking /clear /compact"),
    ("screen_not_contains", "s01_after", "\u2192 help"),
    ("driver_contains", "MARK boot settled"),
    ("restore_markers",),
]
