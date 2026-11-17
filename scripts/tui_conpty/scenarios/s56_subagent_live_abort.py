# -*- coding: utf-8 -*-
# Real-TUI acceptance for a LIVE subagent run.
#
# The mock parent calls the real `subagent` tool with the builtin `worker`
# agent; the child's first mocked turn runs a 30 s bash command, so the run
# stays live while the scenario opens /subagents, expands it, aborts it with
# `x`, and verifies the aborted notice + clean close.
NAME = "s56_subagent_live_abort"
OFFLINE = False
TIMEOUT = 150
MOCK_SCRIPT = [
    {"tool_call": {"name": "subagent",
                   "arguments": {"agent": "worker",
                                 "task": "sleep for a long time"}}},
    {"tool_call": {"name": "bash",
                   "arguments": {"command": "sleep 30"}}},
    {"text": "parent saw the abort"},
]
OPS = [
    ("wait_quiet", 4),
    ("log", "spawn a live worker subagent"),
    ("send", b"spawn a worker subagent\r"),
    ("wait", 7.0),
    ("snap", "s56_running"),
    ("log", "open /subagents while the child is running"),
    ("send", b"/subagents\r"),
    ("wait", 1.2),
    ("snap", "s56_overlay_open"),
    ("log", "expand the selected run"),
    ("send", b"\r"),
    ("wait", 0.5),
    ("snap", "s56_overlay_expanded"),
    ("log", "abort the selected run"),
    ("send", b"x"),
    ("wait", 0.6),
    ("snap", "s56_aborting"),
    ("wait", 4.0),
    ("snap", "s56_aborted"),
    ("send", b"\x1b"),
    ("wait", 1.0),
    ("snap", "s56_closed"),
    ("send", b"\x7f" * 20),
    ("wait", 0.6),
]
CHECKS = [
    ("screen_contains", "s56_running", "1 running"),
    ("screen_contains", "s56_running", "worker"),
    ("screen_contains", "s56_overlay_open", "Subagents · 1 running"),
    ("screen_contains", "s56_overlay_expanded", "sleep for a long time"),
    ("screen_contains", "s56_aborted", "Subagent worker aborted"),
    ("screen_not_contains", "s56_closed", "↑/↓ select"),
    ("restore_markers",),
]
