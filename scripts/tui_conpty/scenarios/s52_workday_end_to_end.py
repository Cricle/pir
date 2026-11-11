# -*- coding: utf-8 -*-
# W30-TUI task A — "one workday" as a repeatable battery scenario.
#
# REAL TASK, HERMETIC PROVIDER. A fresh user machine is simulated: a fake
# HOME/USERPROFILE and a fresh PIR_AGENT_DIR under the scenario dir, first
# launch with no history, and a small real Python project whose unittest
# FAILS. The scripted mock LLM drives the conversation, but every product
# surface under test is real: the TUI, todo panel, read/edit/bash tools, the
# real file bytes, memory persistence (cm store), the subagent child turn,
# `/tree`, session JSONL, and the `--continue` relaunch.
#
# Flow (mock conversation):
#   plan        -> todo create x2
#   fix         -> bash (tests FAIL) -> read calc.py -> edit -> bash (tests OK)
#                  -> todo #1 completed
#   memory      -> remember kind=decision
#   subagent    -> child bash writes subagent_note.txt; parent reports
#   /tree       -> Session Tree overlay
#   /quit       -> driver graceful quit, then OPS2 relaunch with --continue
#   --continue  -> history/todo restored; answers RESUMED-OK
#
# The prompt with a path-like token (`subagent_note.txt`) is sent as SPLIT
# text + delayed `\r` (round29 observation O1: the file-completion popup
# consumes the first Enter when text+\r arrive in one ConPTY write).
#
# On-disk assertions (files/tests/session) run in the driver process after
# both legs via VERIFY() — the battery's check vocabulary is screen-only.
NAME = "s52_workday_end_to_end"
OFFLINE = False  # the app must reach the injected mock over HTTP
TIMEOUT = 220
TIMEOUT2 = 120

import glob as _glob
import json as _json
import os as _os
import subprocess as _subprocess

_PYTHON = "C:/wp/empty/tools/python312/python.exe"

_CALC_BUGGY = """def add(a, b):
    # BUG: subtraction instead of addition
    return a - b
"""

_TEST_CALC = """import unittest

from calc import add


class TestAdd(unittest.TestCase):
    def test_add_positive(self):
        self.assertEqual(add(2, 3), 5)

    def test_add_zero(self):
        self.assertEqual(add(0, 0), 0)


if __name__ == "__main__":
    unittest.main()
"""

_README = "# workday demo\n\nSmall arithmetic project used by a pir integration journey.\n"

_TEST_CMD = '"%s" -m unittest -v' % _PYTHON

MOCK_SCRIPT = [
    # turn 1: plan -> two todo creates
    {"tool_calls": [
        {"name": "todo", "arguments": {"action": "create", "subject": "Fix failing test in calc.py"}},
        {"name": "todo", "arguments": {"action": "create", "subject": "Verify the test suite passes"}},
    ]},
    {"text": "TODO-READY: two tasks created."},
    # turn 2: observe failure, read, edit, verify, complete
    {"tool_call": {"name": "bash", "arguments": {"command": _TEST_CMD}}},
    {"tool_call": {"name": "read", "arguments": {"path": "calc.py"}}},
    {"tool_call": {"name": "edit", "arguments": {
        "path": "calc.py",
        "edits": [{"oldText": "    # BUG: subtraction instead of addition\n    return a - b",
                   "newText": "    return a + b"}]}}},
    {"tool_call": {"name": "bash", "arguments": {"command": _TEST_CMD}}},
    {"tool_call": {"name": "todo", "arguments": {"action": "update", "id": 1, "status": "completed"}}},
    {"text": "FIXED-OK: calc.py add() now returns a + b and the unittest suite passes."},
    # turn 3: memory decision
    {"tool_call": {"name": "memory", "arguments": {
        "action": "remember", "kind": "decision", "scope": "repo",
        "subject": "calc.py arithmetic contract",
        "text": "calc.add must implement addition (a + b); regression test test_calc.py covers it.",
        "source": "agent:pir:round30-workday"}}},
    {"text": "MEMORY-OK: decision recorded."},
    # turn 4: subagent does a small real task
    {"tool_call": {"name": "subagent", "arguments": {
        "agent": "helper",
        "task": "Create a file subagent_note.txt in the current directory containing the single line: helper was here"}}},
    # child request: bash write
    {"tool_call": {"name": "bash", "arguments": {
        "command": "printf 'helper was here\\n' > subagent_note.txt"}}},
    # child final
    {"text": "SUBAGENT-CHILD-DONE: wrote subagent_note.txt"},
    # parent final after subagent result
    {"text": "SUBAGENT-OK: helper created subagent_note.txt."},
    # --continue leg: answer the resume question
    {"text": "RESUMED-OK: earlier we fixed calc.py add() to a + b, recorded the memory decision, and the helper wrote subagent_note.txt."},
]

ENV = {
    # Fake HOME: config must not fall back to the operator's real ~/.pi.
    "HOME": "{agent_dir}/.home",
    "USERPROFILE": "{agent_dir}/.home",
}

OPS = [
    ("wait_quiet", 5.0),
    ("snap", "s52_boot"),
    ("log", "prompt1: plan the work with todo"),
    ("send", b"Plan the work: fix the failing test in calc.py and record the project decision.\r"),
    ("wait_quiet", 5.5),
    ("snap", "s52_plan"),
    ("log", "prompt2: run tests, read/edit the fix, verify"),
    ("send", b"Now fix the failing test in calc.py, run the test suite, and mark the plan done.\r"),
    ("wait_quiet", 8.0),
    ("snap", "s52_fix"),
    ("log", "prompt3: memory decision"),
    ("send", b"Record this project decision in memory: calc.add must implement addition.\r"),
    ("wait_quiet", 5.0),
    ("snap", "s52_memory"),
    ("log", "prompt4 (split send): subagent helper task; path-like token opens file completion"),
    ("send", b"Use the helper subagent to write subagent_note.txt with 'helper was here'."),
    ("wait", 0.6),
    # split text/\r alone is not enough: the file-completion popup is still
    # open and would consume Enter (round30 finding). Esc dismisses the popup
    # without touching the editor text; the following Enter then submits.
    ("send", b"\x1b"),
    ("wait", 0.3),
    ("send", b"\r"),
    ("wait_quiet", 10.0),
    ("snap", "s52_subagent"),
    ("log", "/tree state view (one write, as round29 tree probe)"),
    ("send", b"/tree\r"),
    ("wait", 2.0),
    ("snap", "s52_tree"),
    ("send", b"\x1b"),
    ("wait", 1.0),
    ("send", b"\x7f" * 80),
    ("wait", 0.8),
]

OPS2 = [
    ("wait_quiet", 6.0),
    ("snap", "s52_cont_boot"),
    ("log", "ask the resumed session what happened"),
    ("send", b"What did we fix earlier today?\r"),
    ("wait_quiet", 5.0),
    ("snap", "s52_cont_answer"),
    ("send", b"\x7f" * 80),
    ("wait", 0.8),
]
ARGS2 = ["--continue"]

CHECKS = [
    # control: nothing from the task before the first prompt
    ("screen_not_contains", "s52_boot", "TODO-READY"),
    ("screen_not_contains", "s52_boot", "FIXED-OK"),
    # todo plan visible
    ("screen_contains", "s52_plan", "TODO-READY"),
    ("screen_contains", "s52_plan", "2 pending"),
    ("screen_contains", "s52_plan", "#1 Fix failing test in calc.py"),
    # real edit + real test run after the fix
    ("screen_contains", "s52_fix", "edit calc.py"),
    ("screen_contains", "s52_fix", "Ran 2 tests"),
    ("screen_contains", "s52_fix", "OK"),
    ("screen_contains", "s52_fix", "1/2 completed"),
    ("screen_contains", "s52_fix", "FIXED-OK"),
    # memory persisted (tool result ok)
    ("screen_contains", "s52_memory", '"ok": true'),
    ("screen_contains", "s52_memory", "MEMORY-OK"),
    # subagent panel + /tree overlay
    ("screen_contains", "s52_tree", "subagent helper —"),
    ("screen_contains", "s52_tree", "● helper · done"),
    ("screen_contains", "s52_tree", "SUBAGENT-OK"),
    ("screen_contains", "s52_tree", "Session Tree"),
    ("screen_contains", "s52_tree", "1/2 completed"),
    # --continue replays the session and the todo state
    ("screen_contains", "s52_cont_answer", "RESUMED-OK"),
    ("screen_contains", "s52_cont_answer", "1/2 completed"),
    ("restore_markers",),
]


# ---------------------------------------------------------------------------
# SETUP: real user fixture tree (the per-scenario agent dir exists only at
# run time, so the project is created here, not at module import).
# ---------------------------------------------------------------------------
def _write(path, text):
    _os.makedirs(_os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)


def _long(path):
    """Verbatim Win32 path (this host has LongPathsEnabled=0; battery session
    paths are ~300 chars)."""
    absolute = _os.path.abspath(path)
    if _os.name != "nt" or absolute.startswith("\\\\"):
        return path
    return "\\\\?\\" + absolute


def SETUP(agent_dir):
    _write(_os.path.join(agent_dir, "calc.py"), _CALC_BUGGY)
    _write(_os.path.join(agent_dir, "test_calc.py"), _TEST_CALC)
    _write(_os.path.join(agent_dir, "README.md"), _README)
    _write(_os.path.join(agent_dir, "agents", "helper.md"),
           "---\nname: helper\ndescription: Workday helper agent for small file tasks\n---\n"
           "You are a careful helper. Do the task and report briefly.\n")
    _os.makedirs(_os.path.join(agent_dir, ".home"), exist_ok=True)
    before = _subprocess.run([_PYTHON, "-m", "unittest", "-v"], cwd=agent_dir,
                             capture_output=True, text=True, timeout=60)
    _write(_os.path.join(agent_dir, "tests_before.txt"),
           "exit=%d\n--- stdout ---\n%s\n--- stderr ---\n%s\n"
           % (before.returncode, before.stdout, before.stderr))
    _write(_os.path.join(agent_dir, "setup.json"),
           _json.dumps({"before_test_exit": before.returncode}))


# ---------------------------------------------------------------------------
# VERIFY: on-disk outcomes (files, test exit codes, session JSONL, cm store,
# second leg exit/restore, no panic/mojibake in any raw TUI log).
# ---------------------------------------------------------------------------
def verify_on_disk(scen_dir):
    agent = _os.path.join(scen_dir, "agent")
    out = {}

    try:
        setup = _json.loads(open(_long(_os.path.join(agent, "setup.json")),
                                 encoding="utf-8").read())
        rc = setup.get("before_test_exit")
        out["before_test_failed"] = (rc not in (0, None), f"initial unittest exit={rc}")
    except (OSError, ValueError) as exc:
        out["before_test_failed"] = (False, f"cannot read setup.json: {exc}")

    after = _subprocess.run([_PYTHON, "-m", "unittest", "-v"], cwd=agent,
                            capture_output=True, text=True, timeout=60)
    tail = (after.stdout + after.stderr).strip().splitlines()[-2:]
    out["after_test_passed"] = (after.returncode == 0,
                                f"final unittest exit={after.returncode}; tail={tail}")

    try:
        calc = open(_long(_os.path.join(agent, "calc.py")), encoding="utf-8").read()
        out["calc_fixed"] = ("a + b" in calc and "a - b" not in calc,
                             "calc.py has 'a + b' and no 'a - b'")
    except OSError as exc:
        out["calc_fixed"] = (False, f"cannot read calc.py: {exc}")

    note_path = _os.path.join(agent, "subagent_note.txt")
    try:
        note = open(_long(note_path), encoding="utf-8").read()
        out["subagent_note"] = (note.strip() == "helper was here", repr(note))
    except OSError as exc:
        out["subagent_note"] = (False, f"cannot read subagent_note.txt: {exc}")

    session_files = _glob.glob(_os.path.join(_long(agent), "sessions", "*", "*.jsonl"))
    names, user_texts, total_bytes = [], [], 0
    for path in session_files:
        try:
            text = open(path, encoding="utf-8", errors="replace").read()
        except OSError:
            continue
        total_bytes += len(text.encode("utf-8"))
        for line in text.splitlines():
            try:
                entry = _json.loads(line)
            except ValueError:
                continue
            if entry.get("type") != "message":
                continue
            msg = entry.get("message") or {}
            content = msg.get("content")
            if msg.get("role") == "assistant" and isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "toolCall":
                        names.append(block.get("name"))
            elif msg.get("role") == "user":
                if isinstance(content, str):
                    user_texts.append(content)
                elif isinstance(content, list):
                    user_texts.extend(b.get("text", "") for b in content
                                      if isinstance(b, dict))
    want = {"todo", "read", "edit", "bash", "memory", "subagent"}
    out["session_tools"] = (want.issubset(set(names)),
                            f"tool calls={sorted(set(names))} in {len(session_files)} file(s)")
    out["session_jsonl_size"] = (len(session_files) >= 1 and total_bytes > 10_000,
                                 f"{total_bytes} B across {len(session_files)} file(s)")
    out["session_resume_prompt"] = (
        any("What did we fix earlier today?" in t for t in user_texts),
        f"user turns={len(user_texts)}")

    cm_hits = 0
    for path in _glob.glob(_os.path.join(_long(agent), "sessions", "*", "cm")):
        try:
            cm_hits += open(path, "rb").read().count(b"calc.add must implement addition")
        except OSError:
            pass
    out["memory_store_persisted"] = (cm_hits >= 1, f"decision text x{cm_hits} in cm store")

    leg2_path = _os.path.join(scen_dir, NAME + "_continue.result.json")
    try:
        leg2 = _json.loads(open(_long(leg2_path), encoding="utf-8").read())
        counts = leg2.get("counts") or {}
        ok = (leg2.get("exit_code") == 0 and not leg2.get("timed_out")
              and counts.get("2004l") == 1 and counts.get("25h", 0) >= 1
              and not any(m.startswith("WARN: still alive") for m in leg2.get("marks", [])))
        out["continue_exit_restore"] = (
            ok, f"exit={leg2.get('exit_code')} timed_out={leg2.get('timed_out')} "
                f"2004l={counts.get('2004l')} 25h={counts.get('25h')} "
                f"marks={leg2.get('marks')}")
    except (OSError, ValueError) as exc:
        out["continue_exit_restore"] = (False, f"cannot read leg2 result: {exc}")

    panic, mojibake = [], []
    for path in _glob.glob(_os.path.join(scen_dir, "*.log")):
        try:
            raw = open(_long(path), "rb").read()
        except OSError:
            continue
        low = raw.lower()
        if b"panicked at" in low or b"thread 'main' panicked" in low:
            panic.append(_os.path.basename(path))
        if raw.decode("utf-8", "replace").count("\ufffd"):
            mojibake.append(_os.path.basename(path))
    out["no_panic"] = (not panic, f"panic in {panic}" if panic else "no panic in logs")
    out["no_mojibake"] = (not mojibake, f"U+FFFD in {mojibake}" if mojibake else "no U+FFFD")
    return out


VERIFY = verify_on_disk
