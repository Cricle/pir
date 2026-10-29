# -*- coding: utf-8 -*-
# Ported from r2b_s2_shifttab.py. Intent: ESC[Z (BackTab / shift+tab) cycles
# the thinking level (no Enter involved, so no model call happens).
# CALIBRATED 2026-10-15 (observed against the fixed target-alt build): with a
# configured model the worker answers with WorkerEvent::ThinkingCycled and the
# app only repaints the footer/border -- the "Thinking level: <level>" NOTICE
# exists solely on the headless fallback path (worker inactive) and never
# renders in the TUI. Assertions: boot footer shows a level, the footer row
# CHANGED after the key, and the after-footer still carries a level token.
#
# 2026-10-16: the level TOKEN depends on the seeded `defaultThinkingLevel`
# (the seeded settings say "max"): the interactive session now honours it (the
# startup-level fix), so one BackTab wraps max -> off and the footer renders
# the "thinking off" label (`app.rs` footer) instead of the bare `off`. The
# checks therefore accept both label forms rather than pinning one level.
NAME = "s02_thinking_shifttab"
OPS = [
    ("wait_quiet", 4),
    ("log", "boot footer shows the thinking level"),
    ("snap", "s02_boot"),
    ("log", "shift+tab (ESC[Z) must cycle thinking"),
    ("send", b"\x1b[Z"),
    ("wait", 1.5),
    ("snap", "s02_after"),
]
LEVEL_TOKEN = r"(thinking off|off|minimal|low|medium|high|xhigh|max)"
CHECKS = [
    ("screen_regex", "s02_boot",
     rf"gpt-5 • {LEVEL_TOKEN}"),
    ("screen_row_changed", "s02_boot", "s02_after", r"gpt-5 • "),
    ("screen_regex", "s02_after",
     rf"gpt-5 • {LEVEL_TOKEN}"),
    ("restore_markers",),
]
