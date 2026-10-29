# -*- coding: utf-8 -*-
# W13-M / Task B part 2a — "session lifecycle: new -> switch -> quit", the
# FIRST half of the /sessions-tabs + persistence path. The second half (a
# fresh process resuming the same agent dir) is s47b; the two scenarios share
# the run's `<scenario dir>/agent` directory by contract:
#
#   run_battery.py gives every scenario its own dir, and s47b's OPS write a
#   small marker file NEXT TO its own agent dir with the path s47's session
#   file lives at, then drive `--session-file`-style resume through the
#   documented `/resume` picker (no `--session` flag: the picker path is what a
#   user actually uses). Both scenarios are classified `SEED_AGENT = True`, so
#   each gets its own hermetic mock; s47b's mock still answers the resume-time
#   replay (the replay is local, no request is made) and any later prompt.
#
# WHAT IT DRIVES (one hermetic mock conversation per request):
#   req 1 (prompt 1 in tab 1)  -> ONE bash tool call `echo ALPHA-TOOL-47`
#   req 2 (its result)         -> text "ALPHA-TURN-47"
#   req 3 (prompt 2 in tab 2)  -> text "BETA-TURN-47"
#   req 4 (anything later)     -> text "FOLLOW-47"
#
# Then: `/new` (a second tab with an empty transcript), `/sessions 1` (switch
# back and REPLAY tab 1 from its session FILE, tool block included), and a
# plain `/quit` from the scenario (the driver's own quit path then finds the
# app already exiting, or one process to stop — either way a `RETRY:` mark
# fails the scenario, so the single-`/quit` rule is enforced for the real
# user-visible quit too).
#
# Assertion shape: every check is a pyte render of the live screen. The tool
# block is asserted through its call line (`$ echo ALPHA-TOOL-47`) AND its
# result row (`ALPHA-TOOL-47` alone on a row, which the call line cannot
# satisfy because the call line prefix is `$ `), so "the tool block survived
# the switch" cannot pass on a stray echo of the prompt.
NAME = "s47_session_tabs_quit"
OFFLINE = False
TIMEOUT = 180
# No fixture session: s47b resumes THIS scenario's real session file, and a
# fixture row in the same directory would only add picker noise.
SEED_SESSION = False

MOCK_SCRIPT = [
    {"tool_call": {"name": "bash",
                   "arguments": {"command": "echo ALPHA-TOOL-47"}}},
    {"text": "ALPHA-TURN-47"},
    {"text": "BETA-TURN-47"},
    {"text": "FOLLOW-47"},
]

OPS = [
    ("wait_quiet", 4.0),
    ("snap", "s47_boot"),
    ("log", "tab 1 (startup session): one bash tool call, then the turn's answer"),
    ("send", b"Run the alpha check.\r"),
    ("wait", 10.0),
    ("wait_quiet", 5.0),
    ("snap", "s47_tab1_done"),
    ("log", "/new: a second session tab, empty transcript"),
    ("send", b"/new\r"),
    ("wait", 4.0),
    ("snap", "s47_new_tab"),
    ("log", "prompt in tab 2 so it has its own transcript"),
    ("send", b"Beta prompt only.\r"),
    ("wait", 8.0),
    ("wait_quiet", 4.0),
    ("snap", "s47_tab2_done"),
    ("log", "/sessions 1: switch back to tab 1 (replay from its session file)"),
    ("send", b"/sessions 1\r"),
    ("wait", 4.0),
    ("wait_quiet", 3.0),
    ("snap", "s47_back_to_tab1"),
    ("log", "the user quits with one /quit (the driver then only waits)"),
    ("send", b"/quit\r"),
    ("wait", 4.0),
]

CHECKS = [
    # ---- control: nothing from either turn exists before the prompt ----
    ("screen_not_contains", "s47_boot", "ALPHA-TURN-47"),
    ("screen_not_contains", "s47_boot", "BETA-TURN-47"),

    # ---- tab 1: the tool block and the turn's answer are on screen ----
    ("screen_contains", "s47_tab1_done", "$ echo ALPHA-TOOL-47"),
    ("screen_regex", "s47_tab1_done", r"(?m)^\s*ALPHA-TOOL-47\s*$"),
    ("screen_contains", "s47_tab1_done", "ALPHA-TURN-47"),

    # ---- /new: the old transcript is GONE and the footer moved to the NEW
    # tab. `/new` appends a tab (it does not replace tab 1), so the active
    # position is 2 of 2 — the FIRST observed render of this snapshot showed
    # exactly that (`• [2/2] 2026-10-22T02-50...`), so this is calibration,
    # not a guess: the assertion pins "the tab indicator follows the new,
    # empty session". ----
    ("screen_contains", "s47_new_tab", "New session started"),
    ("screen_not_contains", "s47_new_tab", "ALPHA-TURN-47"),
    ("screen_not_contains", "s47_new_tab", "ALPHA-TOOL-47"),
    ("screen_regex", "s47_new_tab", r"\[2/2\]"),

    # ---- tab 2 has its own transcript, still no alpha content ----
    ("screen_contains", "s47_tab2_done", "Beta prompt only."),
    ("screen_contains", "s47_tab2_done", "BETA-TURN-47"),
    ("screen_not_contains", "s47_tab2_done", "ALPHA-TURN-47"),

    # ---- switching back restores tab 1's transcript INCLUDING the tool
    # block, and the footer follows the active tab ----
    ("screen_contains", "s47_back_to_tab1", "ALPHA-TURN-47"),
    ("screen_contains", "s47_back_to_tab1", "$ echo ALPHA-TOOL-47"),
    ("screen_regex", "s47_back_to_tab1", r"(?m)^\s*ALPHA-TOOL-47\s*$"),
    ("screen_not_contains", "s47_back_to_tab1", "BETA-TURN-47"),
    ("screen_regex", "s47_back_to_tab1", r"\[1/2\]"),

    # ---- quit: one `/quit` exits (a driver RETRY mark fails the scenario) ----
    ("restore_markers",),
]
