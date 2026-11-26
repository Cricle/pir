# -*- coding: utf-8 -*-
# REAL-PROVIDER plan-mode smoke test (configured ZCode LLM).
# Enters /mode plan, asks the REAL model for a small plan. The plan-mode
# system prompt instructs it to call submit_plan; verify the approval appears,
# approve it, and confirm the session switches to normal mode.
NAME = "r03_real_plan_smoke"
NEEDS_REAL_PROVIDER = True
OFFLINE = False
TIMEOUT = 300
OPS = [
    ("wait_quiet", 4),
    ("send", b"/mode plan\r"),
    ("wait", 1.5),
    ("snap", "r03_plan_on"),
    ("log", "ask the real model to plan and submit"),
    ("send", b"Use submit_plan to submit a short 2-step plan for creating a file named real_plan.txt. Set verification to the array [\"test -f real_plan.txt\"] (an array of command strings). Do not do anything else.\r"),
    ("wait", 30.0),
    ("wait_quiet", 8),
    ("snap", "r03_pending"),
    ("log", "open the approval dialog and approve"),
    ("send", b"/approvals\r"),
    ("wait", 2.0),
    ("snap", "r03_dialog"),
    ("send", b"\r"),
    ("wait", 2.0),
    ("snap", "r03_approved"),
    ("log", "wait for the approved-plan follow-up run to settle before quitting"),
    ("wait", 20.0),
    ("wait_quiet", 6),
    ("snap", "r03_executed"),
    ("send", b"/quit\r"),
]
CHECKS = [
    ("screen_contains", "r03_plan_on", "MODE: PLAN"),
    ("screen_contains", "r03_pending", "PLAN APPROVAL"),
    ("screen_contains", "r03_dialog", "Plan & approvals"),
    ("screen_contains", "r03_approved", "Approved plan"),
    ("screen_not_contains", "r03_approved", "MODE: PLAN"),
    ("screen_not_contains", "r03_executed", "MODE: PLAN"),
    ("restore_markers",),
]
