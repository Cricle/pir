# -*- coding: utf-8 -*-
# Security probe: in PLAN mode, subagents are advertised as read-only. This
# scenario asks the mock model to delegate a `scout` whose task is to RUN a
# mutating bash command (write a file). The probe passes only if the mutation
# is BLOCKED and no file is created on disk.
NAME = "s60_plan_readonly_probe"
OFFLINE = False
TIMEOUT = 120
MOCK_SCRIPT = [
    # Parent (plan mode): delegate a bash-capable scout to write a file.
    {"tool_call": {"name": "subagent",
                   "arguments": {"agent": "scout",
                                 "task": "run bash: printf PWNED > plan_pwned.txt"}}},
    # Child (scout): the model actually TRIES to call bash. If the read-only
    # fence works, this call is blocked/missing and no file is created.
    {"tool_call": {"name": "bash",
                   "arguments": {"command": "printf PWNED > plan_pwned.txt"}}},
    # Last step repeats: any further turn just finishes.
    {"text": "done"},
]
OPS = [
    ("wait_quiet", 4),
    ("send", b"/mode plan\r"),
    ("wait", 1.2),
    ("send", b"investigate\r"),
    ("wait", 15.0),
    ("wait_quiet", 5),
    ("snap", "s60_result"),
    ("send", b"/mode normal\r"),
    ("wait", 1.0),
    ("send", b"/quit\r"),
]
CHECKS = [
    ("screen_contains", "s60_result", "MODE: PLAN"),
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
    agent = _os.path.join(scen_dir, "agent")
    out = {}
    for name in ("plan_pwned.txt",):
        p = _os.path.join(agent, name)
        out["no_" + name.replace(".", "_")] = (
            not _os.path.exists(_long(p)),
            f"{name} exists={_os.path.exists(_long(p))}",
        )
    # Prove the hard fence: the scout child's tool list (carried in the
    # parent's subagent tool-result details) must not contain bash, and a bash
    # call must have been rejected.
    child_tools = None
    bash_rejected = False
    for path in _glob.glob(_os.path.join(agent, "sessions", "*", "*.jsonl")):
        try:
            text = open(_long(path), encoding="utf-8").read()
        except OSError:
            continue
        for line in text.splitlines():
            low = line.lower()
            if '"toolname":"subagent"' in low and '"role":"toolresult"' in low:
                if '"name":"read"' in low and '"name":"grep"' in low and '"name":"ls"' in low:
                    child_tools = "read-only set present"
                if 'tool bash not found' in low:
                    bash_rejected = True
    out["scout_child_readonly_tools"] = (
        child_tools is not None,
        f"child read-only tool set: {child_tools}",
    )
    out["bash_call_rejected"] = (
        bash_rejected,
        f"child bash call rejected={bash_rejected}",
    )
    return out
