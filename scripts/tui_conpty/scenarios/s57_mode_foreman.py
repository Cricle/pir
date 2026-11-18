# -*- coding: utf-8 -*-
# Real-TUI acceptance for the user-triggered foreman mode.
#
# The mock model tries a direct `bash` call after /mode foreman. The mode
# builtin must block it at the tool_call seam and show the delegation reason.
# The model then delegates to a real `subagent` worker (which writes a proof
# file in its normal-mode child session), tries to finish WITHOUT a reviewer,
# gets sent back by the agent_before_settle gate, calls a real `reviewer`
# subagent that replies PASS, and only then finishes. /mode normal must clear
# the footer badge; /mode plan must enable the read-only plan mode and clear
# again on /mode normal.
NAME = "s57_mode_foreman"
OFFLINE = False
TIMEOUT = 180
MOCK_SCRIPT = [
    # 1. The foreman tries a direct bash call: it must be blocked.
    {"tool_call": {"name": "bash", "arguments": {"command": "printf blocked"}}},
    # 2. The foreman delegates to the real `subagent` tool.
    {"tool_call": {"name": "subagent",
                   "arguments": {"agent": "worker",
                                 "task": "write the worker proof file"}}},
    # 3-4. The worker child runs in normal mode and writes the proof. The
    # paced text keeps the run observable long enough for the snapshots.
    {"tool_call": {"name": "bash",
                   "arguments": {"command": "printf WORKER_OK > worker.txt"}}},
    {"text": "worker done", "chunks": 20, "chunk_delay_ms": 100},
    # 5-6. The foreman greps the proof (read-only tools stay available; a
    # collapsed `read` hides its body, while grep renders the matching line).
    {"tool_call": {"name": "grep",
                   "arguments": {"pattern": "WORKER_OK", "path": "worker.txt"}}},
    # 7. The foreman tries to finish without a reviewer: the settle gate must
    #    inject FOREMAN GATE and continue the run.
    {"text": "foreman turn complete", "chunks": 10, "chunk_delay_ms": 100},
    # 8. After the gate, the foreman calls the real reviewer subagent.
    {"tool_call": {"name": "subagent",
                   "arguments": {"agent": "reviewer",
                                 "task": "verify worker.txt contains WORKER_OK and reply PASS or FAIL with evidence"}}},
    # 9. The reviewer child replies PASS (paced so the gate footer stays
    #    observable before the verdict clears it).
    {"text": "PASS\nEvidence: worker.txt contains WORKER_OK.", "chunks": 40,
     "chunk_delay_ms": 100},
    # 10. The foreman can now finish.
    {"text": "review PASS: WORKER_OK verified; foreman complete", "chunks": 10,
     "chunk_delay_ms": 100},
]
OPS = [
    ("wait_quiet", 4),
    ("log", "enter foreman mode"),
    ("send", b"/mode foreman\r"),
    ("wait", 1.2),
    ("snap", "s57_foreman_on"),
    ("log", "send a prompt; the mock model tries a direct bash call"),
    ("send", b"do the task\r"),
    ("wait", 1.8),
    ("snap", "s57_blocked"),
    ("log", "wait for the premature finish and the settle gate"),
    ("wait", 4.0),
    ("snap", "s57_gate"),
    ("log", "wait for the reviewer PASS and the final answer"),
    ("wait", 9.0),
    ("wait_quiet", 3),
    ("snap", "s57_reviewer"),
    ("log", "return to normal mode"),
    ("send", b"/mode normal\r"),
    ("wait", 1.2),
    ("snap", "s57_normal"),
    ("log", "plan mode is a read-only investigation mode"),
    ("send", b"/mode plan\r"),
    ("wait", 1.2),
    ("snap", "s57_plan"),
    ("log", "leave plan mode"),
    ("send", b"/mode normal\r"),
    ("wait", 1.2),
    ("snap", "s57_plan_normal"),
]
CHECKS = [
    ("screen_contains", "s57_foreman_on", "MODE: FOREMAN"),
    ("screen_contains", "s57_foreman_on", "Foreman mode enabled"),
    ("screen_contains", "s57_blocked", "Foreman mode: direct tool"),
    ("screen_contains", "s57_blocked", "Delegate this work"),
    ("screen_contains", "s57_gate", "WORKER_OK"),
    ("screen_contains", "s57_gate", "FOREMAN GATE"),
    ("screen_contains", "s57_reviewer", "review PASS"),
    ("screen_contains", "s57_reviewer", "PHASE: VERIFIED"),
    ("screen_contains", "s57_normal", "Normal mode restored"),
    ("screen_not_contains", "s57_normal", "MODE: FOREMAN"),
    ("screen_not_contains", "s57_normal", "PHASE:"),
    ("screen_contains", "s57_plan", "MODE: PLAN"),
    ("screen_contains", "s57_plan", "Plan mode enabled"),
    ("screen_not_contains", "s57_plan", "MODE: FOREMAN"),
    ("screen_contains", "s57_plan_normal", "Normal mode restored"),
    ("screen_not_contains", "s57_plan_normal", "MODE: PLAN"),
    ("screen_not_contains", "s57_plan_normal", "PHASE:"),
    ("restore_markers",),
]
