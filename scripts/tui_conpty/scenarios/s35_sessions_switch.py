# -*- coding: utf-8 -*-
# Same-window multi-session tabs (2026-10-15): `/sessions` switching.
#
# Two sessions are started in one TUI window: the startup session (tab 1) is
# named `alpha` and receives a prompt, then `/new` creates tab 2 (`beta`) with
# its own prompt. `/sessions 1` / `/sessions 2` switch back and forth through
# the existing session-attach machinery; each snapshot asserts the replayed
# transcript of the selected session and the footer's `[n/N] title` tab
# indicator. Mock-LLM only (the battery injects the hermetic provider).
#
# Note: the scenario is a `.py` data module like every other file in this
# directory — `run_battery.py` discovers `scenarios/*.py` (`NAME` + `OPS` +
# `CHECKS`); there is no JSON scenario loader in this harness.
NAME = "s35_sessions_switch"
SEED_SESSION = True
TIMEOUT = 180
OPS = [
    ("wait_quiet", 4),
    ("snap", "s35_boot"),
    ("log", "name the startup session (tab 1)"),
    ("send", b"/name alpha\r"),
    ("wait", 2.0),
    ("log", "prompt in alpha"),
    ("send", b"alpha first prompt\r"),
    ("wait", 8.0),
    ("snap", "s35_alpha"),
    ("log", "/new starts a second session (tab 2)"),
    ("send", b"/new\r"),
    ("wait", 3.0),
    ("snap", "s35_new"),
    ("send", b"/name beta\r"),
    ("wait", 2.0),
    ("log", "prompt in beta"),
    ("send", b"beta second prompt\r"),
    ("wait", 8.0),
    ("snap", "s35_beta"),
    ("log", "/sessions 1 switches back to alpha"),
    ("send", b"/sessions 1\r"),
    ("wait", 3.0),
    ("snap", "s35_back_to_alpha"),
    ("log", "/sessions 2 switches back to beta"),
    ("send", b"/sessions 2\r"),
    ("wait", 3.0),
    ("snap", "s35_back_to_beta"),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
]
CHECKS = [
    # Tab 1 carried its own transcript.
    ("screen_contains", "s35_alpha", "alpha first prompt"),
    # The second tab is live after /new and carries its own transcript; the
    # footer shows the active position/title. The footer truncates its TAIL at
    # the terminal width (upstream `footer.ts:286` truncates `pwd` the same
    # way), so a long out-root cuts the title after `[n/N]`; the session name
    # before the marker still proves the indicator follows the active tab.
    ("screen_contains", "s35_beta", "beta second prompt"),
    ("screen_contains", "s35_beta", "beta • [2/2]"),
    # Switching back restores the first session's transcript AND the footer
    # marker follows the active tab.
    ("screen_contains", "s35_back_to_alpha", "alpha first prompt"),
    ("screen_contains", "s35_back_to_alpha", "alpha • [1/2]"),
    ("screen_contains", "s35_back_to_alpha", "mock reply"),
    # ... and forward again.
    ("screen_contains", "s35_back_to_beta", "beta second prompt"),
    ("screen_contains", "s35_back_to_beta", "beta • [2/2]"),
    # Alt-screen cycles are not observable through ConPTY (it consumes
    # ESC[?1049h/l); the clean exit is the gate.
    ("restore_markers",),
]
