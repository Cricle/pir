# -*- coding: utf-8 -*-
# New (battery starter) - THE FREEZE REGRESSION TEST. Intent: typing a unique
# plain token and pressing Enter must SUBMIT - i.e. the editor must clear.
# The check is scoped to the editor region (the input box between its two box
# rules), because on success the submitted text legitimately reappears in the
# transcript; on the frozen build the token just sits in the editor.
# CALIBRATED 2026-10-15: after Enter the editor's TOP border carries the
# "Working" label (`── ⠇ Working ─────…`), so run_battery's editor_region now
# accepts labeled border rows (previously "editor region not found"). Observed
# post-Enter rows: transcript " hi-battery-7q" above, then the labeled top
# border, one empty editor row, the full-width bottom border, cwd + status.
# SEED_AGENT=False: keep the submit offline/deterministic (no provider
# configured -> the post-submit model leg fails locally, no gateway call).
# Cleanup after the check: Esc (abort if a run started), ctrl+c (clear editor),
# backspaces, so the driver's /quit is never polluted by leftover text.
NAME = "s13_submit_clears_editor"
SEED_AGENT = False
TIMEOUT = 150
OPS = [
    ("wait_quiet", 4),
    ("log", "type unique token (no slash -> no completion popup)"),
    ("send", b"hi-battery-7q"),
    ("wait", 1.2),
    ("snap", "s13_typed"),
    ("log", "Enter must submit"),
    ("send", b"\r"),
    ("wait", 2.5),
    ("snap", "s13_after_enter"),
    ("log", "cleanup: esc abort, ctrl+c clear, backspaces"),
    ("send", b"\x1b"),
    ("wait", 1.0),
    ("send", b"\x03"),
    ("wait", 0.8),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
    ("snap", "s13_before_quit"),
]
CHECKS = [
    ("editor_contains", "s13_typed", "hi-battery-7q"),
    ("editor_not_contains", "s13_after_enter", "hi-battery-7q"),
    ("restore_markers",),
]
