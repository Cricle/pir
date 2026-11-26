# -*- coding: utf-8 -*-
# Plan-mode acceptance: submit_plan -> approval -> automatic switch to normal
# -> the approved plan is executed with full tools.
#
# Entering /mode plan hides mutating tools and exposes `submit_plan`. The mock
# model submits a plan, the tool call is blocked at the gate with a pending
# approval, and /approvals opens the dialog. Approving sends a follow-up that
# runs in normal mode and writes THE_PLAN_OK via bash; MODE: PLAN must clear.
NAME = "s59_mode_plan"
OFFLINE = False
TIMEOUT = 240
MOCK_SCRIPT = [
    # 1. The planner submits a concrete plan for approval.
    {"tool_call": {"name": "submit_plan",
                   "arguments": {"plan": "1. write the plan proof file\n2. verify it",
                                 "verification": ["test -f plan.txt"]}}},
    # 2. The planner settles and waits for the decision.
    {"text": "waiting for plan approval", "chunks": 8, "chunk_delay_ms": 100},
    # 3. After approval, the follow-up runs in NORMAL mode and implements.
    {"tool_call": {"name": "bash",
                   "arguments": {"command": "printf THE_PLAN_OK > plan.txt"}}},
    # 4. Final answer after implementation.
    {"text": "plan implemented: THE_PLAN_OK verified", "chunks": 10,
     "chunk_delay_ms": 100},
]
OPS = [
    ("wait_quiet", 4),
    ("log", "enter plan mode; mutating tools are hidden"),
    ("send", b"/mode plan\r"),
    ("wait", 1.2),
    ("snap", "s59_plan_on"),
    ("log", "send a prompt that attempts a direct mutation (must be blocked)"),
    ("send", b"make a plan\r"),
    ("wait", 3.0),
    ("snap", "s59_blocked"),
    ("log", "wait for the plan submission to settle with a pending approval"),
    ("wait_quiet", 3),
    ("snap", "s59_pending"),
    ("log", "approve the submitted plan"),
    ("send", b"/approvals\r"),
    ("wait", 1.5),
    ("snap", "s59_dialog"),
    ("send", b"\r"),
    ("wait", 1.5),
    ("snap", "s59_approved"),
    ("log", "wait for the approved plan to execute in normal mode"),
    ("wait", 10.0),
    ("wait_quiet", 3),
    ("snap", "s59_implemented"),
    ("send", b"/mode normal\r"),
    ("wait", 1.2),
    ("snap", "s59_normal"),
]
CHECKS = [
    ("screen_contains", "s59_plan_on", "MODE: PLAN"),
    ("screen_contains", "s59_plan_on", "Plan mode enabled"),
    ("screen_contains", "s59_pending", "PLAN APPROVAL"),
    ("screen_contains", "s59_pending", "Run /approvals"),
    ("screen_contains", "s59_dialog", "Plan & approvals"),
    ("screen_contains", "s59_dialog", "plan"),
    ("screen_contains", "s59_approved", "Approved plan"),
    ("screen_not_contains", "s59_approved", "MODE: PLAN"),
    ("screen_contains", "s59_implemented", "THE_PLAN_OK"),
    ("screen_not_contains", "s59_implemented", "MODE: PLAN"),
    ("screen_contains", "s59_normal", "Already in normal mode."),
    ("screen_not_contains", "s59_normal", "MODE: PLAN"),
    ("restore_markers",),
]
