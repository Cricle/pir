# -*- coding: utf-8 -*-
# New (P0 input regression). Intent: in the EDITOR (input box region between
# its two box rules), typing `abc` and then sending the DEL byte (0x7f -- what
# a real Windows console puts on the input stream for Backspace) must DELETE
# the last character, i.e. the editor must read `ab` and no longer `abc`.
# The failure this pins: startup installs ENABLE_VIRTUAL_TERMINAL_INPUT on the
# console input handle, crossterm 0.29's Windows INPUT_RECORD parser cannot
# round-trip the console's VT translation, and Backspace goes dead (the byte
# is dropped or surfaces as a literal character) -- the same mismatch behind
# the reported "Enter/Backspace unusable, weird symbols typed" bug.
# SEED_AGENT=False: keep the scenario offline/deterministic (no provider
# configured; nothing is submitted, so no model leg can start).
# Cleanup after the checks: Esc (abort if anything started), ctrl+c (clear the
# editor), backspaces, so the driver's /quit is never polluted by leftovers.
NAME = "s15_backspace_deletes"
SEED_AGENT = False
OPS = [
    ("wait_quiet", 4),
    ("log", "type abc (no slash -> no completion popup)"),
    ("send", b"abc"),
    ("wait", 1.2),
    ("snap", "s15_typed"),
    ("log", "backspace (0x7f) must delete the last character"),
    ("send", b"\x7f"),
    ("wait", 1.2),
    ("snap", "s15_after_backspace"),
    ("log", "cleanup: esc abort, ctrl+c clear, backspaces"),
    ("send", b"\x1b"),
    ("wait", 1.0),
    ("send", b"\x03"),
    ("wait", 0.8),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
    ("snap", "s15_before_quit"),
]
CHECKS = [
    ("editor_contains", "s15_typed", "abc"),
    ("editor_contains", "s15_after_backspace", "ab"),
    ("editor_not_contains", "s15_after_backspace", "abc"),
    ("restore_markers",),
]
