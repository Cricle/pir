# -*- coding: utf-8 -*-
# Real-provider A/B probe: does the model burn wall-clock on blind sleep/poll
# while waiting for an unknown external event?
#
# We enter foreman mode with MANUAL approvals (the gate blocks the first worker
# delegation), then ask the model to delegate a worker and "wait for approval".
# Correct behavior after the no-blind-wait rule: issue the subagent call, END the
# turn, and let the approval arrive as a follow-up. Blind behavior: bash sleep /
# repeated background_status / poll loops.
NAME = "r06_no_wait_probe"
NEEDS_REAL_PROVIDER = True
OFFLINE = False
TIMEOUT = 240
OPS = [
    ("wait_quiet", 4),
    ("send", b"/mode foreman\r"),
    ("wait", 2.0),
    ("send", b"/foreman approvals manual\r"),
    ("wait", 2.0),
    ("log", "ask the real model to delegate and wait for an approval event"),
    ("send", b"Use the subagent tool with agent worker to write DONE to wait_probe.txt. A user approval may be required; after you issue the tool call, wait for whatever comes next. Do not use read tools.\r"),
    # Give the model time to call subagent and reach the approval gate; no
    # further input from us during this window, so any sleep/poll is the model's.
    ("wait", 55.0),
    ("wait_quiet", 10),
    ("snap", "r06_after_wait"),
    ("send", b"/quit\r"),
]
CHECKS = [
    ("screen_contains", "r06_after_wait", "FOREMAN APPROVAL"),
    ("restore_markers",),
]
