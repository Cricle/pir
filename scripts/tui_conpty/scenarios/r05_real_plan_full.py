# -*- coding: utf-8 -*-
# REAL-PROVIDER plan FULL task: the real LLM plans via submit_plan, the user
# approves, the session switches to Normal, and the approved-plan follow-up
# actually creates the file with the real tool surface. On-disk VERIFY proves
# both the approved plan text and the executed file.
NAME = "r05_real_plan_full"
NEEDS_REAL_PROVIDER = True
OFFLINE = False
TIMEOUT = 600

OPS = [
    ("wait_quiet", 4),
    ("send", b"/mode plan\r"),
    ("wait", 1.5),
    ("snap", "r05_on"),
    ("log", "real model submits a plan"),
    ("send", b"Use submit_plan to submit a one-step plan: create a file r05_plan.txt in the current directory containing exactly PLAN_REAL_OK. Do not use any other tool.\r"),
    ("wait", 60.0),
    ("wait_quiet", 10),
    ("snap", "r05_pending"),
    ("log", "approve the plan"),
    ("send", b"/approvals\r"),
    ("wait", 2.0),
    ("snap", "r05_dialog"),
    ("send", b"\r"),
    ("wait", 2.0),
    ("snap", "r05_approved"),
    ("log", "wait for the approved-plan follow-up to actually create the file"),
    ("wait", 60.0),
    ("wait_quiet", 10),
    ("snap", "r05_executed"),
    ("send", b"/mode normal\r"),
    ("wait", 1.5),
    ("snap", "r05_normal"),
    ("send", b"/quit\r"),
]

CHECKS = [
    ("screen_contains", "r05_on", "MODE: PLAN"),
    ("screen_contains", "r05_pending", "PLAN APPROVAL"),
    ("screen_contains", "r05_dialog", "Plan & approvals"),
    ("screen_contains", "r05_approved", "Approved plan"),
    ("screen_not_contains", "r05_approved", "MODE: PLAN"),
    ("screen_not_contains", "r05_executed", "MODE: PLAN"),
    ("screen_contains", "r05_executed", "PLAN_REAL_OK"),
    ("screen_contains", "r05_normal", "Already in normal mode."),
    ("restore_markers",),
]

import glob as _glob
import os as _os


def _long(path):
    absolute = _os.path.abspath(path)
    if _os.name != "nt" or absolute.startswith("\\\\"):
        return path
    return "\\\\?\\" + absolute


def VERIFY(scen_dir):
    out = {}
    agent = _os.path.join(scen_dir, "agent")
    proof = _os.path.join(agent, "r05_plan.txt")
    try:
        text = open(_long(proof), encoding="utf-8").read().strip()
        out["plan_executed_file"] = (text == "PLAN_REAL_OK", f"r05_plan.txt={text!r}")
    except OSError as exc:
        out["plan_executed_file"] = (False, f"missing: {exc}")

    submitted = False
    approved_normal = False
    for path in _glob.glob(_os.path.join(agent, "sessions", "*", "*.foreman.jsonl")):
        try:
            text = open(_long(path), encoding="utf-8").read()
        except OSError:
            continue
        low = text.lower()
        if 'submit_plan' in low or 'plan_submitted' in low or '"kind":"plan"' in low:
            submitted = True
        # After approval the journal snapshot has mode normal, an approved plan
        # record, and no pending work (events may be compacted into the
        # snapshot, so check the replayed STATE, not the raw event names).
        if '"mode":"normal"' in low and '"kind":"plan"' in low and '"state":"approved"' in low:
            approved_normal = True
    out["plan_submitted_in_journal"] = (submitted, f"submit_plan seen={submitted}")
    out["plan_approval_switched_journal"] = (
        approved_normal,
        f"plan_submitted + mode normal in journal={approved_normal}",
    )
    return out
