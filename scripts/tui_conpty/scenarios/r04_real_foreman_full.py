# -*- coding: utf-8 -*-
# REAL-PROVIDER foreman FULL task: the real LLM delegates a worker that writes
# a file on disk, then an independent reviewer that must reply PASS, then the
# final answer. Uses the real subagent tool with the real model (children
# inherit the parent provider/credential/model).
#
# Strong deterministic hints are in the prompt; the on-disk file and the
# session JSONL are asserted by VERIFY, not just by screen text.
NAME = "r04_real_foreman_full"
NEEDS_REAL_PROVIDER = True
OFFLINE = False
TIMEOUT = 900

OPS = [
    ("wait_quiet", 4),
    ("send", b"/mode foreman\r"),
    ("wait", 1.5),
    ("snap", "r04_on"),
    ("log", "real foreman task: delegate worker, then reviewer, then finish"),
    ("send", b"Delegate ONE worker subagent (agent=worker) with this task: using its bash tool, create a file named r04_worker.txt in the current directory containing exactly WORKER_REAL_OK. Then immediately delegate ONE reviewer subagent (agent=reviewer) whose task is: run `cat r04_worker.txt`, and reply with PASS on the very first line if the file contains WORKER_REAL_OK, otherwise FAIL. Reply with only PASS or FAIL plus one short evidence line. Finally reply with exactly FOREMAN_DONE. Do not use any other tools and do not write files yourself.\r"),
    ("wait", 180.0),
    ("wait_quiet", 15),
    ("snap", "r04_result"),
    ("send", b"/mode normal\r"),
    ("wait", 1.5),
    ("snap", "r04_normal"),
    ("send", b"/quit\r"),
]

CHECKS = [
    ("screen_contains", "r04_on", "MODE: FOREMAN"),
    ("screen_contains", "r04_result", "WORKER_REAL_OK"),
    ("screen_contains", "r04_result", "PASS"),
    ("screen_contains", "r04_result", "PHASE: VERIFIED"),
    ("screen_contains", "r04_normal", "Normal mode restored"),
    ("screen_not_contains", "r04_normal", "MODE: FOREMAN"),
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
    sub_calls = {"worker": 0, "reviewer": 0}
    proof = _os.path.join(agent, "r04_worker.txt")
    try:
        text = open(_long(proof), encoding="utf-8").read().strip()
        out["worker_file"] = (text == "WORKER_REAL_OK", f"r04_worker.txt={text!r}")
    except OSError as exc:
        out["worker_file"] = (False, f"missing: {exc}")

    reviewer_pass = False
    for path in _glob.glob(_os.path.join(agent, "sessions", "*", "*.jsonl")):
        try:
            text = open(_long(path), encoding="utf-8").read()
        except OSError:
            continue
        for line in text.splitlines():
            low = line.lower()
            if '"name":"subagent"' in low or '"toolname":"subagent"' in low:
                if '"worker"' in low:
                    sub_calls["worker"] += 1
                if '"reviewer"' in low:
                    sub_calls["reviewer"] += 1
            # A reviewer verdict is an assistant text beginning with PASS.
            if '"role":"assistant"' in low and '"pass' in low:
                reviewer_pass = True
    out["worker_delegated"] = (sub_calls["worker"] >= 1, f"worker calls={sub_calls['worker']}")
    out["reviewer_delegated"] = (sub_calls["reviewer"] >= 1, f"reviewer calls={sub_calls['reviewer']}")
    out["reviewer_pass_in_transcript"] = (reviewer_pass, f"assistant PASS seen={reviewer_pass}")
    return out
