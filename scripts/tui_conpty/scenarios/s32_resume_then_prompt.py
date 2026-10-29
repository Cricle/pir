# -*- coding: utf-8 -*-
# User report (2026-10-16): after `/resume` the transcript does not refresh and
# a prompt sent afterwards produces NOTHING at all (no stream, no text, no
# error). The existing resume scenarios do not cover this: s14 cancels the
# picker without resuming, s26 resumes but only asserts restore markers. This
# scenario resumes the seeded fixture for real, asserts the replayed transcript
# is on screen, then sends a prompt and asserts the mock reply renders.
#
# Seeded fixture (run_battery.py::seed_session_fixture): one user message
# "seeded battery fixture session". Mock LLM default reply: "mock reply".
NAME = "s32_resume_then_prompt"
SEED_SESSION = True
TIMEOUT = 120
OPS = [
    ("wait_quiet", 4),
    ("snap", "s32_boot"),
    ("log", "/resume opens the picker"),
    ("send", b"/resume\r"),
    ("wait", 2.5),
    ("snap", "s32_picker"),
    ("log", "Enter resumes the first listed session"),
    ("send", b"\r"),
    ("wait", 3.0),
    ("snap", "s32_resumed"),
    ("log", "prompt after resume must run and stream a reply"),
    ("send", b"hello after resume\r"),
    ("wait", 8.0),
    ("snap", "s32_after_prompt"),
    ("log", "second prompt: the session must stay usable"),
    ("send", b"again\r"),
    ("wait", 8.0),
    ("snap", "s32_after_second"),
]
CHECKS = [
    ("screen_contains", "s32_picker", "Resume a session"),
    # the resumed session's transcript is replayed into the view
    ("screen_contains", "s32_resumed", "seeded battery fixture session"),
    # a prompt sent after resume actually runs and streams
    ("screen_contains", "s32_after_prompt", "mock reply"),
    ("screen_contains", "s32_after_second", "mock reply"),
    # The picker owns its own alternate screen (session_pick.rs:363 enter /
    # :367 leave) and the surface re-enters afterwards; those 1049 cycles are
    # not observable through ConPTY (it consumes ESC[?1049h/l), so the check
    # keeps its teeth on the invariants that matter (clean exit, bracketed
    # paste off exactly once, cursor shown).
    ("restore_markers",),
]
