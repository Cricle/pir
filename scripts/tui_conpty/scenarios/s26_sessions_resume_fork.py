# Promoted from the 2026-10-15 play session (p12_sessions.py) into the
# acceptance battery: a real-user regression scenario. Its evidence
# lives in scripts/tui_conpty/play/ and WAVE-REPORT.md §6b.
# -*- coding: utf-8 -*-
# PLAY p12 — sessions a user juggles: /resume picker (list, navigate, cancel,
# reopen, Enter to actually resume), then /fork on the resumed session.
NAME = "s26_sessions_resume_fork"
SEED_SESSION = True
TIMEOUT = 150
OPS = [
    ("wait_quiet", 4),
    ("send", b"/resume\r"),
    ("wait", 2.5),
    ("snap", "p12_picker"),
    ("send", b"\x1b[B"),
    ("wait", 0.8),
    ("snap", "p12_picker_down"),
    ("send", b"\x1b[B"),
    ("wait", 0.8),
    ("snap", "p12_picker_down2"),
    ("send", b"\x1b[A"),
    ("wait", 0.8),
    ("send", b"\x1b"),
    ("wait", 1.5),
    ("snap", "p12_cancelled"),
    ("send", b"/resume\r"),
    ("wait", 2.5),
    ("send", b"\r"),
    ("wait", 3.0),
    ("snap", "p12_resumed"),
    ("send", b"/fork\r"),
    ("wait", 2.5),
    ("snap", "p12_forked"),
    ("send", b"/name p12-name\r"),
    ("wait", 2.0),
    ("snap", "p12_named"),
    ("send", b"/session\r"),
    ("wait", 2.0),
    ("snap", "p12_session_info"),
    ("send", b"\x7f" * 40),
    ("wait", 0.6),
]
CHECKS = [
    # The scenario opens two pickers (/resume twice incl. a cancel, then
    # /fork) whose alt-screen cycles are timing-dependent AND not observable
    # through ConPTY (it consumes ESC[?1049h/l; see run_battery
    # restore_markers). restore_markers gates the clean exit; the
    # single-`/quit` rule is enforced by the runner for every scenario.
    ("restore_markers",),
]
