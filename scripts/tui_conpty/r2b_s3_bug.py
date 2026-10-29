# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/r2b_s3_bug.py (copied 2026-10-15) into
# scripts/tui_conpty/. Scenario data unchanged (provenance header only).
# P2-3: typing "/bug" completes to /bug (exact hit, not /debug); /share and /import
# also registered. Enter executes /bug -> bug-report notice.
NAME = "r2b_s3_bug"
OPS = [
    ("wait_quiet", 4),
    ("log", "type /bug (popup open)"),
    ("send", b"/bug"),
    ("wait", 1.2),
    ("snap", "r2b_s3_bug_popup"),
    ("log", "enter to execute /bug"),
    ("send", b"\r"),
    ("wait", 0.8),
    ("snap", "r2b_s3_bug_enter1"),
    ("send", b"\r"),
    ("wait", 2.0),
    ("snap", "r2b_s3_bug_after"),
    ("log", "type /sh (share popup)"),
    ("send", b"/sh"),
    ("wait", 1.0),
    ("snap", "r2b_s3_share_popup"),
    ("log", "clear, type /import"),
    ("send", b"\x7f\x7f\x7f/import"),
    ("wait", 1.0),
    ("snap", "r2b_s3_import_popup"),
]
