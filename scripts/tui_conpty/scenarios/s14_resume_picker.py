# -*- coding: utf-8 -*-
# New (battery starter) - session picker without a model. Intent: /resume
# executes, the full-screen picker opens ("Resume a session") and LISTS the
# seeded session, Esc cancels it and the TUI surface is restored (header hint
# back, picker title gone). Provider-free: the picker only lists session files.
#
# CALIBRATED 2026-10-15 (run out/20261015-162749, binary mtime 16:17:34):
# * Seeding: the fixture landed in the battery layout only after the runner
#   wrote it through `_win_long_path` (run_battery.py:429-469): the fixture
#   path is ~280 chars and Windows here has LongPathsEnabled=0, so the plain
#   `open()` failed with FileNotFoundError although `os.makedirs` (already
#   prefixed) had created the directory. Observed after the fix: the picker
#   row `seeded battery fixture session  1 now` renders (fixture first
#   message + message count), i.e. the picker listed the seeded session.
# * Alt screen: the picker owns its own alternate screen
#   (`pick_session_with_all`, src/coding_agent/session_pick.rs:363 enter /
#   :367 leave) and the caller re-enters the surface's screen afterwards
#   (src/tui/terminal.rs:8765-8768). Those ESC[?1049h/l cycles are NOT
#   observable through ConPTY (it consumes them; see run_battery
#   restore_markers), so `restore_markers` gates the invariants ConPTY does
#   preserve: exit 0, clean quit, bracketed paste off exactly once, cursor
#   shown.
NAME = "s14_resume_picker"
SEED_SESSION = True
TIMEOUT = 90
OPS = [
    ("wait_quiet", 4),
    ("log", "/resume -> picker opens"),
    ("send", b"/resume\r"),
    ("wait", 2.5),
    ("snap", "s14_picker"),
    ("log", "esc cancels the picker"),
    ("send", b"\x1b"),
    ("wait", 2.0),
    ("snap", "s14_back"),
    ("log", "cleanup editor"),
    ("send", b"\x7f" * 24),
    ("wait", 0.6),
]
CHECKS = [
    ("screen_contains", "s14_picker", "Resume a session"),
    # The seeded session itself must be listed, not just the picker frame.
    ("screen_contains", "s14_picker", "seeded battery fixture session"),
    ("screen_contains", "s14_back", "esc interrupt"),
    ("screen_not_contains", "s14_back", "Resume a session"),
    ("restore_markers",),
]
