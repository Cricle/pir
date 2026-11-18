# -*- coding: utf-8 -*-
# Foreman v2 acceptance: manual approval queue + retry + reviewer gate.
#
# The first worker delegation is blocked by `approvals=manual`; the model
# settles waiting for a decision. `/approvals` opens the select/confirm
# dialogs; approving sends a trigger-turn custom message. The retried worker
# runs (real child subagent), then a real reviewer returns PASS.
NAME = "s58_foreman_v2"
OFFLINE = False
TIMEOUT = 240
MOCK_SCRIPT = [
    # 1. First worker delegation is blocked by the approval gate.
    {"tool_call": {"name": "subagent",
                   "arguments": {"agent": "worker",
                                 "task": "write the approval worker proof"}}},
    # 2. The foreman settles and waits for a decision.
    {"text": "waiting for the user approval", "chunks": 8, "chunk_delay_ms": 100},
    # 3. After /approvals, the retried worker is admitted.
    {"tool_call": {"name": "subagent",
                   "arguments": {"agent": "worker",
                                 "task": "write the approval worker proof"}}},
    # 4-5. Worker child (normal mode): write the proof, then finish.
    {"tool_call": {"name": "bash",
                   "arguments": {"command": "printf APPROVAL_OK > approval.txt"}}},
    {"text": "worker done", "chunks": 10, "chunk_delay_ms": 100},
    # 6. The foreman calls the real reviewer.
    {"tool_call": {"name": "subagent",
                   "arguments": {"agent": "reviewer",
                                 "task": "verify approval.txt contains APPROVAL_OK and reply PASS or FAIL"}}},
    # 7. Reviewer child.
    {"text": "PASS\nEvidence: approval.txt contains APPROVAL_OK.", "chunks": 30,
     "chunk_delay_ms": 100},
    # 8. Foreman final answer.
    {"text": "review PASS: approval flow complete: APPROVAL_OK verified", "chunks": 10,
     "chunk_delay_ms": 100},
]
OPS = [
    ("wait_quiet", 4),
    ("send", b"/mode foreman\r"),
    ("wait", 1.2),
    ("send", b"/foreman approvals manual\r"),
    ("wait", 1.2),
    ("send", b"do the approval task\r"),
    ("wait", 3.0),
    ("snap", "s58_blocked"),
    ("log", "wait for the run to settle with a pending approval"),
    ("wait_quiet", 3),
    ("snap", "s58_pending"),
    ("log", "open the approval queue and approve the first request"),
    ("send", b"/approvals\r"),
    ("wait", 1.5),
    ("snap", "s58_approval_dialog"),
    ("send", b"\r"),
    ("wait", 1.2),
    ("snap", "s58_approved"),
    ("log", "wait for the retried worker and reviewer PASS"),
    ("wait", 10.0),
    ("wait_quiet", 3),
    ("snap", "s58_verified"),
    ("send", b"/mode normal\r"),
    ("wait", 1.2),
    ("snap", "s58_normal"),
]
CHECKS = [
    ("screen_contains", "s58_pending", "FOREMAN APPROVAL"),
    ("screen_contains", "s58_pending", "Run /approvals"),
    ("screen_contains", "s58_pending", "pending"),
    ("screen_contains", "s58_approval_dialog", "Foreman approvals"),
    ("screen_contains", "s58_approved", "APPROVAL APPROVED"),
    ("screen_not_contains", "s58_approved", "FOREMAN APPROVALS"),
    ("screen_contains", "s58_verified", "APPROVAL_OK"),
    ("screen_contains", "s58_verified", "review PASS"),
    ("screen_contains", "s58_verified", "PHASE: VERIFIED"),
    ("screen_not_contains", "s58_verified", "FOREMAN APPROVALS"),
    ("screen_contains", "s58_normal", "Normal mode restored"),
    ("screen_not_contains", "s58_normal", "MODE: FOREMAN"),
    ("screen_not_contains", "s58_normal", "FOREMAN APPROVALS"),
    ("restore_markers",),
]
